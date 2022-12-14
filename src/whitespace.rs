use crate::unicode::{Character, CS};
use crate::utils::py_invalid_type_error;
use anyhow::anyhow;
use itertools::Itertools;
use pyo3::prelude::*;
use regex::{escape, Regex};

#[pyfunction(use_graphemes = "true")]
pub fn remove(s: &str, use_graphemes: bool) -> String {
    CS::new(s, use_graphemes)
        .chars()
        .filter(|c| !c.is_whitespace())
        .join("")
}

#[pyfunction(use_graphemes = "true")]
pub fn full(s: &str, use_graphemes: bool) -> String {
    CS::new(s, use_graphemes)
        .chars()
        .filter(|c| !c.is_whitespace())
        .join(" ")
}

#[derive(Debug, Clone, Copy, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub enum WhitespaceOperation {
    Keep,
    Insert,
    Delete,
}

impl<'a> FromPyObject<'a> for WhitespaceOperation {
    fn extract(ob: &'a PyAny) -> PyResult<Self> {
        let s: PyResult<String> = ob.extract();
        let ws_op = if s.is_ok() {
            match s.unwrap().as_str() {
                "k" | "keep" => WhitespaceOperation::Keep,
                "i" | "insert" => WhitespaceOperation::Insert,
                "d" | "delete" => WhitespaceOperation::Delete,
                k => return Err(py_invalid_type_error(k, "whitespace operation")),
            }
        } else {
            let s: u8 = ob.extract()?;
            match s {
                0 => WhitespaceOperation::Keep,
                1 => WhitespaceOperation::Insert,
                2 => WhitespaceOperation::Delete,
                k => return Err(py_invalid_type_error(k, "whitespace operation")),
            }
        };
        Ok(ws_op)
    }
}

impl IntoPy<PyObject> for WhitespaceOperation {
    fn into_py(self, py: Python<'_>) -> PyObject {
        match self {
            WhitespaceOperation::Keep => "k",
            WhitespaceOperation::Insert => "i",
            WhitespaceOperation::Delete => "d",
        }
        .into_py(py)
    }
}

#[pyfunction(use_graphemes = "true")]
pub fn operations(
    from: &str,
    to: &str,
    use_graphemes: bool,
) -> anyhow::Result<Vec<WhitespaceOperation>> {
    let from_cs = CS::new(from, use_graphemes);
    let to_cs = CS::new(to, use_graphemes);
    let from_chars: Vec<Character> = from_cs.chars().collect();
    let to_chars: Vec<Character> = to_cs.chars().collect();
    let mut operations = vec![];
    operations.reserve(from_chars.len().max(to_chars.len()));
    let mut from_ptr = 0;
    let mut to_ptr = 0;
    while from_ptr < from_chars.len() {
        let from_char = &from_chars[from_ptr];
        let to_char = if to_ptr < to_chars.len() {
            Some(&to_chars[to_ptr])
        } else {
            None
        };
        if to_char.is_some() && from_char == to_char.unwrap() {
            operations.push(WhitespaceOperation::Keep);
            to_ptr += 1;
        } else if to_char.is_some() && to_char.unwrap().is_whitespace() {
            operations.push(WhitespaceOperation::Insert);
            to_ptr += 2;
        } else if from_char.is_whitespace() {
            operations.push(WhitespaceOperation::Delete);
        } else {
            return Err(anyhow!(
                "should not happen, most likely your inputs contain multiple \
                consecutive whitespaces, leading, or trailing whitespaces, \
                prepare them first using the clean function:\n\
                from: \"{from}\"\nto  : \"{to}\"\n\
                from_char: \"{from_char}\"\nto_char  : \"{to_char:?}\"\n"
            ));
        }
        from_ptr += 1;
    }
    Ok(operations)
}

pub fn repair(
    s: &str,
    operations: &[WhitespaceOperation],
    use_graphemes: bool,
) -> anyhow::Result<String> {
    let cs = CS::new(s, use_graphemes);
    let chars: Vec<Character> = cs.chars().collect();
    if chars.len() != operations.len() {
        return Err(anyhow!(
            "expected one operation for every character, but got {} operations and \
        {} characters for input\n{s}",
            operations.len(),
            chars.len()
        ));
    };

    let mut output = String::new();
    for (idx, (char, op)) in chars.iter().zip(operations.iter()).enumerate() {
        if *op == WhitespaceOperation::Insert
            && !char.is_whitespace()
            && (idx == 0 || !chars[idx - 1].is_whitespace())
        {
            output.push(' ');
            output.push_str(char.str);
        } else if *op == WhitespaceOperation::Delete && char.is_whitespace() {
            continue;
        } else {
            output.push_str(char.str);
        }
    }
    Ok(output)
}

#[pyfunction(use_graphemes = "true")]
#[pyo3(name = "repair")]
fn repair_py(
    s: &str,
    operations: Vec<WhitespaceOperation>,
    use_graphemes: bool,
) -> anyhow::Result<String> {
    repair(s, &operations, use_graphemes)
}

#[pyfunction]
pub fn find_substring_ignoring_whitespace(
    s: &str,
    substring: &str,
    use_graphemes: bool,
) -> Option<(usize, usize)> {
    let cs = CS::new(substring, use_graphemes);
    let substring = cs
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| escape(c.str))
        .join(r"\s*");
    let re = Regex::new(substring.as_str()).expect("invalid pattern, should never happen");
    re.find(s).map_or(None, |m| Some((m.start(), m.end())))
}

/// A submodule containing functionality specific to handle whitespaces in text.
pub(super) fn add_submodule(py: Python<'_>, parent_module: &PyModule) -> PyResult<()> {
    let m = PyModule::new(py, "whitespace")?;
    m.add_function(wrap_pyfunction!(find_substring_ignoring_whitespace, m)?)?;
    m.add_function(wrap_pyfunction!(repair_py, m)?)?;
    m.add_function(wrap_pyfunction!(operations, m)?)?;
    m.add_function(wrap_pyfunction!(full, m)?)?;
    m.add_function(wrap_pyfunction!(remove, m)?)?;
    parent_module.add_submodule(m)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::whitespace::{
        find_substring_ignoring_whitespace, full, operations, remove, repair, WhitespaceOperation,
    };

    #[test]
    fn test_remove() {
        assert_eq!(remove(" t   h is is \n\t a tes    t ", true), "thisisatest");
        assert_eq!(remove("", true), "");
    }

    #[test]
    fn test_full() {
        assert_eq!(
            full(" t   h is is \n\t a tes    t ", true),
            "t h i s i s a t e s t"
        );
        assert_eq!(full("", true), "");
    }

    #[test]
    fn test_operations() {
        let from = " t  h isis a test  ";
        let to = "this is a test";
        assert_eq!(
            operations(from, from, true).unwrap(),
            vec![WhitespaceOperation::Keep; from.chars().count()]
        );
        assert_eq!(
            operations(to, to, true).unwrap(),
            vec![WhitespaceOperation::Keep; to.chars().count()]
        );
        assert_eq!(
            operations(from, to, true)
                .unwrap()
                .into_iter()
                .map(|op| op as u8)
                .collect::<Vec<u8>>(),
            vec![2, 0, 2, 2, 0, 2, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2]
        );
    }

    #[test]
    fn test_repair() {
        let from = "t h isis a test";
        let to = "this is a test";
        assert_eq!(
            repair(from, &operations(from, to, true).unwrap(), true).unwrap(),
            to
        );
        assert_eq!(
            repair(to, &operations(to, from, true).unwrap(), true).unwrap(),
            "t h isis a test"
        );
        assert_eq!(
            repair("t", &vec![WhitespaceOperation::Delete,], true,).unwrap(),
            "t"
        );
        assert_eq!(repair("", &vec![], true).unwrap(), "");
    }

    #[test]
    fn test_find_substring_ignoring_whitespace() {
        let s = "this is a test sentence";
        let sub = "  a te s\n t";
        let result = find_substring_ignoring_whitespace(s, sub, true);
        assert!(result.is_some());
        let (start, end) = result.unwrap();
        assert_eq!(start, 8);
        assert_eq!(end, 14);
        assert_eq!(&s[start..end], "a test");
        let result = find_substring_ignoring_whitespace(s, "a t??st", true);
        assert!(result.is_none());
        let s = "this is \" a \\w+ test \" sentence";
        let sub = "\"a \\w+test\"";
        let result = find_substring_ignoring_whitespace(s, sub, true);
        assert!(result.is_some());
    }
}
