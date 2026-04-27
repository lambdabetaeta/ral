use crate::types::*;
use std::sync::Arc;

/// Return an error if `args` has fewer than `min` elements.
pub(crate) fn check_arity(args: &[Value], min: usize, name: &str) -> Result<(), EvalSignal> {
    if args.len() < min {
        let noun = if min == 1 { "argument" } else { "arguments" };
        return Err(sig(format!("{name} requires {min} {noun}")));
    }
    Ok(())
}

/// Extract a `HandleInner` reference from `val`, or return a typed error.
pub(crate) fn expect_handle<'a>(val: &'a Value, cmd: &str) -> Result<&'a HandleInner, EvalSignal> {
    match val {
        Value::Handle(h) => Ok(h),
        other => Err(EvalSignal::Error(
            Error::new(
                format!(
                    "{cmd} expects a Handle, got {} '{other}'",
                    other.type_name()
                ),
                1,
            )
            .with_hint("use spawn to create a handle"),
        )),
    }
}

/// Extract the body and captured scope from a `Thunk`, or return a typed error.
pub(crate) fn expect_thunk<'a>(
    val: &'a Value,
    cmd: &str,
) -> Result<(&'a crate::ir::Comp, &'a Arc<Env>), EvalSignal> {
    match val {
        Value::Thunk { body, captured, .. } => Ok((body, captured)),
        other => Err(EvalSignal::Error(
            Error::new(
                format!("{cmd} expects a Block, got {} '{other}'", other.type_name()),
                1,
            )
            .with_hint(format!("{cmd} requires a block: {cmd} {{ ... }}")),
        )),
    }
}

pub(crate) fn sig(message: impl Into<String>) -> EvalSignal {
    EvalSignal::Error(Error::new(message, 1))
}

pub(crate) fn sig_hint(message: impl Into<String>, hint: impl Into<String>) -> EvalSignal {
    EvalSignal::Error(Error::new(message, 1).with_hint(hint))
}

pub(crate) fn as_list(val: &Value, ctx: &str) -> Result<Vec<Value>, EvalSignal> {
    match val {
        Value::List(items) => Ok(items.clone()),
        _ => Err(sig(format!(
            "{ctx} expects a List, got {}",
            val.type_name()
        ))),
    }
}

pub(crate) fn as_byte_list(val: &Value, ctx: &str) -> Result<Vec<u8>, EvalSignal> {
    if let Value::Bytes(b) = val {
        return Ok(b.clone());
    }
    let items = as_list(val, ctx)?;
    let mut out = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        match item {
            Value::Int(n) if *n >= 0 && *n <= 255 => out.push(*n as u8),
            Value::Int(n) => {
                return Err(sig_hint(
                    format!("{ctx}: byte at index {idx} out of range: {n}"),
                    "bytes must be Int values in range 0..255",
                ));
            }
            _ => {
                return Err(sig_hint(
                    format!(
                        "{ctx}: expected Int at index {idx}, got {}",
                        item.type_name()
                    ),
                    "bytes must be Int values in range 0..255",
                ));
            }
        }
    }
    Ok(out)
}

pub(crate) fn as_map(val: &Value, ctx: &str) -> Result<Vec<(String, Value)>, EvalSignal> {
    match val {
        Value::Map(pairs) => Ok(pairs.clone()),
        _ => Err(sig(format!("{ctx} expects a Map, got {}", val.type_name()))),
    }
}

/// Project a `Map` to its entries; returns `&[]` for any other variant.
pub(crate) fn map_entries(val: &Value) -> &[(String, Value)] {
    match val {
        Value::Map(m) => m,
        _ => &[],
    }
}

/// Project a `List` to its items; returns `&[]` for any other variant.
pub(crate) fn list_entries(val: &Value) -> &[Value] {
    match val {
        Value::List(l) => l,
        _ => &[],
    }
}

/// Look up a key in an association list.
pub(crate) fn get<'a>(map: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Coerce a value to a list of strings; a non-`List` wraps its stringification in a singleton.
pub(crate) fn str_list(v: &Value) -> Vec<String> {
    match v {
        Value::List(items) => items.iter().map(|i| i.to_string()).collect(),
        other => vec![other.to_string()],
    }
}

/// Fold map entries into a `Default` struct; error if `val` is not a map.
pub(crate) fn fold_map<T: Default, V>(
    val: &Value,
    ctx: &str,
    extract: impl Fn(&Value) -> V,
    mut assign: impl FnMut(&mut T, &str, V),
) -> Result<T, EvalSignal> {
    if !matches!(val, Value::Map(_)) {
        return Err(sig(format!("{ctx} expects a Map, got {}", val.type_name())));
    }
    let mut out = T::default();
    for (k, v) in map_entries(val) {
        assign(&mut out, k, extract(v));
    }
    Ok(out)
}

pub(crate) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) => (*x as f64) == *y,
        (Value::Float(x), Value::Int(y)) => *x == (*y as f64),
        (Value::String(x), Value::String(y)) => x == y,
        (Value::List(xs), Value::List(ys)) => {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(a, b)| values_equal(a, b))
        }
        (Value::Map(xs), Value::Map(ys)) => {
            xs.len() == ys.len()
                && xs.iter().all(|(k, v)| {
                    ys.iter()
                        .find(|(yk, _)| yk == k)
                        .map(|(_, yv)| values_equal(v, yv))
                        .unwrap_or(false)
                })
        }
        _ => false,
    }
}

pub(crate) fn str_cmp(
    args: &[Value],
    shell: &mut Shell,
    name: &str,
    f: fn(&str, &str) -> bool,
) -> Result<Value, EvalSignal> {
    if args.len() < 2 {
        return Err(sig(format!("{name} requires 2 arguments")));
    }
    let r = f(&args[0].to_string(), &args[1].to_string());
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

pub(crate) fn arg0_str(args: &[Value]) -> String {
    args.first().map(|v| v.to_string()).unwrap_or_default()
}

pub(crate) fn regex_err(ctx: &str, pattern: &str, full: &str) -> String {
    let cause = full
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("error:"))
        .and_then(|l| l.trim_start().strip_prefix("error:"))
        .map(|s| s.trim())
        .unwrap_or("invalid pattern");
    format!("{ctx}: invalid pattern '{pattern}': {cause}")
}

pub(crate) fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Unit,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => Value::List(arr.iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => Value::Map(
            obj.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

pub(crate) fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Unit => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(n) => serde_json::json!(*n),
        Value::Float(f) => serde_json::json!(*f),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(pairs) => {
            let obj: serde_json::Map<String, serde_json::Value> = pairs
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        Value::Thunk { body, .. } => {
            if let crate::ir::CompKind::Lam { param, .. } = &body.as_ref().kind {
                serde_json::json!({"type": "Block", "param": format!("{param:?}")})
            } else {
                serde_json::json!({"type": "Block"})
            }
        }
        Value::Handle(_) => serde_json::json!({"type": "Handle"}),
        Value::Bytes(b) => {
            serde_json::Value::Array(b.iter().map(|byte| serde_json::json!(*byte)).collect())
        }
    }
}

pub fn value_to_json_pub(v: &Value) -> serde_json::Value {
    value_to_json(v)
}
