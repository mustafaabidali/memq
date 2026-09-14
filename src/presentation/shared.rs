//! Response-local dictionaries preserve every evidence occurrence and its order.
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn project(input: &Value) -> Value {
    let Some(items) = input["items"].as_array() else {
        return input.clone();
    };
    if items.is_empty() {
        return input.clone();
    }
    let mut defaults = json!({
        "untrusted":true, "committed":true, "kind":"json-records",
        "acceptance":"n/a", "attribution":"missing", "native_status":null
    });
    defaults
        .as_object_mut()
        .expect("defaults")
        .retain(|key, _| {
            items
                .iter()
                .filter(|item| sourced(item))
                .all(|item| item.get(key).is_some())
        });
    let mut bodies = Vec::new();
    let mut sources = Vec::new();
    let mut occurrences = Vec::new();
    let mut body_indices = BTreeMap::new();
    let mut source_indices = BTreeMap::new();
    for original in items {
        let mut body = original.clone();
        let source = if sourced(original) {
            let fields = body.as_object_mut().expect("item object");
            let observation = fields.remove("observation").expect("observation");
            let availability = fields.remove("availability");
            let pointer = fields["pointer"].as_str().expect("pointer");
            let (prefix, suffix) = split_pointer(pointer);
            let mut source = json!({"observation":observation,"pointer_prefix":prefix});
            if let Some(availability) = availability {
                source["availability"] = availability;
            }
            fields.insert("pointer".into(), json!(suffix));
            for (key, default) in defaults.as_object().expect("defaults") {
                if fields.get(key) == Some(default) {
                    fields.remove(key);
                }
            }
            let key = serde_json::to_string(&source).expect("JSON value");
            let index = *source_indices.entry(key).or_insert_with(|| {
                sources.push(source);
                sources.len() - 1
            });
            json!(index)
        } else {
            // Placeholders have no source/default inheritance. In particular,
            // forgotten evidence must not become committed or authoritative.
            Value::Null
        };
        let key = serde_json::to_string(&body).expect("JSON value");
        let index = *body_indices.entry(key).or_insert_with(|| {
            bodies.push(body);
            bodies.len() - 1
        });
        occurrences.push(json!([index, source]));
    }
    let mut output = input.clone();
    output["memq"]["encoding"] = json!("shared-v1");
    output["items"] = json!(bodies);
    output["sources"] = json!(sources);
    output["occurrence_fields"] = json!(["item", "source"]);
    output["occurrences"] = json!(occurrences);
    output["item_defaults"] = defaults;
    output["encoding_note"] = json!(
        "occurrences preserve result order as [item index, source index]. \
         Each sourced item inherits item_defaults and its source observation/availability. \
         Its full pointer is source.pointer_prefix + item.pointer. \
         A null source is a literal placeholder. Full details: show with the same IDs and view."
    );
    output
}

fn sourced(item: &Value) -> bool {
    item["observation"].is_object() && item["pointer"].is_string() && item.get("text").is_some()
}

fn split_pointer(pointer: &str) -> (&str, &str) {
    if pointer.starts_with("git:")
        && let Some((index, _)) = pointer.match_indices(':').nth(2)
    {
        pointer.split_at(index + 1)
    } else if pointer.starts_with("worktree:") {
        pointer.split_at(pointer.find('#').unwrap_or(pointer.len()))
    } else {
        ("", pointer)
    }
}
