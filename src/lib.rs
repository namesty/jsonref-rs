//! jsonref dereferences JSONSchema `$ref` attributes and creates a new dereferenced schema.
//!
//! Dereferencing is normally done by a JSONSchema validator in the process of validation, but
//! it is sometimes useful to do this independent of the validator for tasks like:
//!
//! * Analysing a schema programatically to see what field there are.
//! * Programatically modifying a schema.
//! * Passing to tools that create fake JSON data from the schema.
//! * Passing the schema to form generation tools.
//!
//!
//! Example:
//! ```
//! use serde_json::json;
//! use jsonref::JsonRef;
//!
//! let mut simple_example = json!(
//!           {"properties": {"prop1": {"title": "name"},
//!                           "prop2": {"$ref": "#/properties/prop1"}}
//!           }
//!        );
//!
//! let mut jsonref = JsonRef::new();
//!
//! jsonref.deref_value(&mut simple_example).unwrap();
//!
//! let dereffed_expected = json!(
//!     {"properties":
//!         {"prop1": {"title": "name"},
//!          "prop2": {"title": "name"}}
//!     }
//! );
//! assert_eq!(simple_example, dereffed_expected)
//! ```
//!
//! **Note**:  If the JSONSchema has recursive `$ref` only the first recursion will happen.
//! This is to stop an infinate loop.

use serde_json::json;
use serde_json::Value;
use snafu::{ResultExt, Snafu};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io;
use std::mem;
use std::path::PathBuf;
use url::Url;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Could not open schema from {}: {}", filename, source))]
    SchemaFromFile {
        filename: String,
        source: std::io::Error,
    },
    #[snafu(display("Could not open schema from url {}: {}", url, source))]
    SchemaFromUrl { url: String, source: ureq::Error },
    #[snafu(display("Parse error for url {}: {}", url, source))]
    UrlParseError {
        url: String,
        source: url::ParseError,
    },
    #[snafu(display("schema from {} not valid JSON: {}", url, source))]
    SchemaNotJson { url: String, source: std::io::Error },
    #[snafu(display("schema from {} not valid JSON: {}", url, source))]
    SchemaNotJsonSerde {
        url: String,
        source: serde_json::Error,
    },
    #[snafu(display("json pointer {} not found", pointer))]
    JsonPointerNotFound { pointer: String },
    #[snafu(display("{}", "Json Ref Error"))]
    JSONRefError { source: std::io::Error },
}

/// Trait used to remove Json Value's element
pub trait Remove {
    /// Method use to remove element in Json Values
    fn remove(&mut self, json_pointer: &str) -> io::Result<Option<Value>>;
}

impl Remove for serde_json::Value {
    /// # Examples: Remove an element in a table
    /// ```
    /// use serde_json::Value;
    /// use json_value_remove::Remove;
    ///
    /// let mut array1: Value = serde_json::from_str(r#"{"my_table":["a","b","c"]}"#).unwrap();
    /// assert_eq!(Some(Value::String("a".to_string())), array1.remove("/my_table/0").unwrap());
    /// assert_eq!(r#"{"my_table":["b","c"]}"#, array1.to_string());
    /// ```
    /// # Examples: Remove a field from an object
    /// ```
    /// use serde_json::Value;
    /// use json_value_remove::Remove;
    ///
    /// let mut object1: Value = serde_json::from_str(r#"{"field1.0":{"field1.1":"value1.1","field1.2":"value1.2"},"field2.0":"value2.0"}"#).unwrap();
    /// assert_eq!(Some(Value::String("value1.2".to_string())), object1.remove("/field1.0/field1.2").unwrap());
    /// assert_eq!(r#"{"field1.0":{"field1.1":"value1.1"},"field2.0":"value2.0"}"#,object1.to_string());
    /// ```
    fn remove(&mut self, json_pointer: &str) -> io::Result<Option<Value>> {
        let fields: Vec<&str> = json_pointer.split("/").skip(1).collect();

        remove(self, fields)
    }
}

fn remove(json_value: &mut Value, fields: Vec<&str>) -> io::Result<Option<Value>> {
    if fields.is_empty() {
        return Ok(None);
    }

    let mut fields = fields.clone();
    let field = fields.remove(0);

    if field.is_empty() {
        return Ok(None);
    }

    match fields.is_empty() {
        true => match json_value {
            Value::Array(vec) => {
                let index = match field.parse::<usize>() {
                    Ok(index) => index,
                    Err(e) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "{}. Can't find the field '{}' in {}.",
                                e,
                                field,
                                json_value.to_string()
                            ),
                        ))
                    }
                };
                let len = vec.len();
                if index >= len {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "removal index (is {}) should be < len (is {}) from {}",
                            index,
                            len,
                            json_value.to_string()
                        ),
                    ));
                }
                Ok(Some(vec.remove(index)))
            }
            Value::Object(map) => Ok(map.remove(field)),
            _ => Ok(None),
        },
        false => match json_value.pointer_mut(format!("/{}", field).as_str()) {
            Some(json_targeted) => remove(json_targeted, fields),
            None => Ok(None),
        },
    }
}

type Result<T, E = Error> = std::result::Result<T, E>;

/// Main struct that holds configuration for a JSONScheama derefferencing.
///
/// Instantiate with
/// ```
/// use jsonref::JsonRef;
/// let jsonref = JsonRef::new();
/// ```
///
/// Configuration is done through the `set_` methods on the struct.
#[derive(Debug)]
pub struct JsonRef {
    schema_cache: HashMap<String, Value>,
    reference_key: Option<String>,
}

impl JsonRef {
    /// Create a new instance of JsonRef.
    pub fn new() -> JsonRef {
        return JsonRef {
            schema_cache: HashMap::new(),
            reference_key: None,
        };
    }

    /// Set a key to store the data that the `$ref` replaced.
    ///
    /// This example uses `__reference__` as the key.
    ///
    /// ```
    /// # use jsonref::JsonRef;
    /// # let jsonref = JsonRef::new();
    /// use serde_json::json;
    ///
    /// let mut input  = json!(
    ///     {"properties": {"prop1": {"title": "name"},
    ///                     "prop2": {"$ref": "#/properties/prop1", "title": "old_title"}}
    ///     }
    /// );
    ///
    /// let expected = json!(
    ///     {"properties": {"prop1": {"title": "name"},
    ///                     "prop2": {"title": "name", "__reference__": {"title": "old_title"}}}
    ///     }
    /// );
    ///                                                                                          
    /// let mut jsonref = JsonRef::new();
    ///
    /// jsonref.set_reference_key("__reference__");
    ///
    /// jsonref.deref_value(&mut input).unwrap();
    ///                                                                                          
    /// assert_eq!(input, expected)
    /// ```

    pub fn set_reference_key(&mut self, reference_key: &str) {
        self.reference_key = Some(reference_key.to_owned());
    }

    /// deref a serde_json value directly. Uses the current working directory for any relative
    /// refs.
    pub fn deref_value(&mut self, value: &mut Value) -> Result<()> {
        let anon_file_url = format!(
            "file://{}/anon.json",
            env::current_dir()
                .context(JSONRefError {})?
                .to_string_lossy()
        );
        self.schema_cache
            .insert(anon_file_url.clone(), value.clone());

        let mut definitions = json!({});

        self.deref(value, anon_file_url, &vec![], &mut definitions)?;
        Ok(())
    }

    /// deref from a URL:
    ///
    /// ```
    /// # use jsonref::JsonRef;
    /// # let jsonref = JsonRef::new();
    /// # use serde_json::Value;
    /// # use std::fs;
    /// let mut jsonref = JsonRef::new();
    /// # jsonref.set_reference_key("__reference__");
    /// let input_url = jsonref.deref_url("https://gist.githubusercontent.com/kindly/91e09f88ced65aaca1a15d85a56a28f9/raw/52f8477435cff0b73c54aacc70926c101ce6c685/base.json").unwrap();
    /// # let file = fs::File::open("fixtures/nested_relative/expected.json").unwrap();
    /// # let file_expected: Value = serde_json::from_reader(file).unwrap();
    /// # assert_eq!(input_url, file_expected)
    /// ```
    pub fn deref_url(&mut self, url: &str) -> Result<Value> {
        let mut value: Value = ureq::get(url)
            .call()
            .context(SchemaFromUrl {
                url: url.to_owned(),
            })?
            .into_json()
            .context(SchemaNotJson {
                url: url.to_owned(),
            })?;

        self.schema_cache.insert(url.to_string(), value.clone());
        let mut definitions = json!({});
        self.deref(&mut value, url.to_string(), &vec![], &mut definitions)?;
        Ok(value)
    }

    /// deref from a File:
    ///
    /// ```
    /// # use jsonref::JsonRef;
    /// # let jsonref = JsonRef::new();
    /// # use serde_json::Value;
    /// # use std::fs;
    ///
    /// let mut jsonref = JsonRef::new();
    /// # jsonref.set_reference_key("__reference__");
    /// let file_example = jsonref
    ///     .deref_file("fixtures/nested_relative/base.json")
    ///     .unwrap();
    /// # let file = fs::File::open("fixtures/nested_relative/expected.json").unwrap();
    /// # let file_expected: Value = serde_json::from_reader(file).unwrap();
    /// # assert_eq!(file_example, file_expected)
    /// ```
    pub fn deref_file(&mut self, file_path: &str) -> Result<Value> {
        let file = fs::File::open(file_path).context(SchemaFromFile {
            filename: file_path.to_owned(),
        })?;
        let mut value: Value = serde_json::from_reader(file).context(SchemaNotJsonSerde {
            url: file_path.to_owned(),
        })?;
        let path = PathBuf::from(file_path);
        let absolute_path = fs::canonicalize(path).context(JSONRefError {})?;
        let url = format!("file://{}", absolute_path.to_string_lossy());

        self.schema_cache.insert(url.clone(), value.clone());
        let mut definitions = json!({});
        self.deref(&mut value, url, &vec![], &mut definitions)?;

        let val = value.as_object_mut().unwrap();
        val.insert("definitions".to_string(), definitions);

        Ok(value)
    }

    fn deref(
        &mut self,
        value: &mut Value,
        id: String,
        used_refs: &Vec<String>,
        definitions: &mut Value,
    ) -> Result<()> {
        let mut new_id = id;
        if let Some(id_value) = value.get("$id") {
            if let Some(id_string) = id_value.as_str() {
                new_id = id_string.to_string()
            }
        }

        if let Some(obj) = value.as_object_mut() {
            if let Some(defs) = obj.get_mut("definitions") {
                let mut defs_clone = defs.clone();
                obj.remove("definitions").unwrap();

                if let Some(def_obj) = defs_clone.as_object_mut() {
                    let accumulated_defs = definitions.as_object_mut().unwrap();
                    for (key, val) in def_obj.iter_mut() {
                        accumulated_defs.insert(key.to_string(), val.clone());
                    }
                }
            }

            if let Some(ref_value) = obj.remove("$ref") {
                if let Some(ref_string) = ref_value.as_str() {
                    let id_url = Url::parse(&new_id).context(UrlParseError {
                        url: new_id.clone(),
                    })?;
                    let ref_url = id_url.join(ref_string).context(UrlParseError {
                        url: ref_string.to_owned(),
                    })?;

                    let mut ref_url_no_fragment = ref_url.clone();
                    ref_url_no_fragment.set_fragment(None);
                    let ref_no_fragment = ref_url_no_fragment.to_string();

                    let mut schema = match self.schema_cache.get(&ref_no_fragment) {
                        Some(cached_schema) => cached_schema.clone(),
                        None => {
                            if ref_no_fragment.starts_with("http") {
                                ureq::get(&ref_no_fragment)
                                    .call()
                                    .context(SchemaFromUrl {
                                        url: ref_no_fragment.clone(),
                                    })?
                                    .into_json()
                                    .context(SchemaNotJson {
                                        url: ref_no_fragment.clone(),
                                    })?
                            } else if ref_no_fragment.starts_with("file") {
                                let file = fs::File::open(ref_url_no_fragment.path()).context(
                                    SchemaFromFile {
                                        filename: ref_no_fragment.clone(),
                                    },
                                )?;
                                serde_json::from_reader(file).context(SchemaNotJsonSerde {
                                    url: ref_no_fragment.clone(),
                                })?
                            } else {
                                panic!("need url to be a file or a http based url")
                            }
                        }
                    };

                    if !self.schema_cache.contains_key(&ref_no_fragment) {
                        self.schema_cache
                            .insert(ref_no_fragment.clone(), schema.clone());
                    }

                    let ref_url_string = ref_url.to_string();
                    if let Some(ref_fragment) = ref_url.fragment() {
                        schema = schema.pointer(ref_fragment).ok_or(
                            Error::JsonPointerNotFound {pointer: format!("ref `{}` can not be resolved as pointer `{}` can not be found in the schema", ref_string, ref_fragment)}
                            )?.clone();
                    }
                    if used_refs.contains(&ref_url_string) {
                        return Ok(());
                    }

                    let mut new_used_refs = used_refs.clone();
                    new_used_refs.push(ref_url_string);

                    self.deref(&mut schema, ref_no_fragment, &new_used_refs, definitions)?;
                    let old_value = mem::replace(value, schema);

                    if let Some(reference_key) = &self.reference_key {
                        if let Some(new_obj) = value.as_object_mut() {
                            new_obj.insert(reference_key.clone(), old_value);
                        }
                    }
                }
            }
        }

        if let Some(obj) = value.as_object_mut() {
            for obj_value in obj.values_mut() {
                self.deref(obj_value, new_id.clone(), used_refs, definitions)?
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::JsonRef;
    use serde_json::{json, Value};
    use std::fs;

    #[test]
    fn json_no_refs() {
        let no_ref_example = json!({"properties": {"prop1": {"title": "proptitle"}}});

        let mut jsonref = JsonRef::new();

        let mut input = no_ref_example.clone();

        jsonref.deref_value(&mut input).unwrap();

        assert_eq!(input, no_ref_example)
    }

    #[test]
    fn json_with_recursion() {
        let mut simple_refs_example = json!(
            {"properties": {"prop1": {"$ref": "#"}}}
        );

        let simple_refs_expected = json!(
            {"properties": {"prop1": {"properties": {"prop1": {}}}}
            }
        );

        let mut jsonref = JsonRef::new();
        jsonref.deref_value(&mut simple_refs_example).unwrap();
        jsonref.set_reference_key("__reference__");

        println!(
            "{}",
            serde_json::to_string_pretty(&simple_refs_example).unwrap()
        );

        assert_eq!(simple_refs_example, simple_refs_expected)
    }

    #[test]
    fn simple_from_url() {
        let mut simple_refs_example = json!(
            {"properties": {"prop1": {"title": "name"},
                            "prop2": {"$ref": "https://gist.githubusercontent.com/kindly/35a631d33792413ed8e34548abaa9d61/raw/b43dc7a76cc2a04fde2a2087f0eb389099b952fb/test.json", "title": "old_title"}}
            }
        );

        let simple_refs_expected = json!(
            {"properties": {"prop1": {"title": "name"},
                            "prop2": {"title": "title from url", "__reference__": {"title": "old_title"}}}
            }
        );

        let mut jsonref = JsonRef::new();
        jsonref.set_reference_key("__reference__");
        jsonref.deref_value(&mut simple_refs_example).unwrap();

        assert_eq!(simple_refs_example, simple_refs_expected)
    }

    #[test]
    fn nested_with_ref_from_url() {
        let mut simple_refs_example = json!(
            {"properties": {"prop1": {"title": "name"},
                            "prop2": {"$ref": "https://gist.githubusercontent.com/kindly/35a631d33792413ed8e34548abaa9d61/raw/0a691c035251f742e8710f71ba92ead307823385/test_nested.json"}}
            }
        );

        let simple_refs_expected = json!(
            {"properties": {"prop1": {"title": "name"},
                            "prop2": {"__reference__": {},
                                      "title": "title from url",
                                      "properties": {"prop1": {"title": "sub property title in url"},
                                                     "prop2": {"__reference__": {}, "title": "sub property title in url"}}
                            }}
            }
        );

        let mut jsonref = JsonRef::new();
        jsonref.set_reference_key("__reference__");
        jsonref.deref_value(&mut simple_refs_example).unwrap();

        assert_eq!(simple_refs_example, simple_refs_expected)
    }

    #[test]
    fn nested_ref_from_local_file() {
        let mut jsonref = JsonRef::new();
        jsonref.set_reference_key("__reference__");
        let file_example = jsonref
            .deref_file("fixtures/nested_relative/base.json")
            .unwrap();

        let file = fs::File::open("fixtures/nested_relative/expected.json").unwrap();
        let file_expected: Value = serde_json::from_reader(file).unwrap();

        println!("{}", serde_json::to_string_pretty(&file_example).unwrap());

        assert_eq!(file_example, file_expected)
    }

    #[test]
    fn test_defs() {
        let mut jsonref = JsonRef::new();
        jsonref.set_reference_key("__reference__");
        let file_example = jsonref
            .deref_file("fixtures/definitions/base.json")
            .unwrap();

        let file = fs::File::open("fixtures/definitions/expected.json").unwrap();
        let file_expected: Value = serde_json::from_reader(file).unwrap();

        println!("{}", serde_json::to_string_pretty(&file_example).unwrap());

        assert_eq!(file_example, file_expected)
    }
}
