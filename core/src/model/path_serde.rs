//! Lossless JSON for file system paths. `PathBuf`'s own serde support
//! fails on a path that is not valid UTF-8 — and on Linux a directory
//! named with arbitrary bytes is legal — which would make a checkpoint
//! fail and lose the queue. Valid UTF-8 stays a plain string (so existing
//! manifests and hand-edits keep working); anything else is stored as
//! `{"bytes": [..]}` (Unix) or `{"wide": [..]}` (Windows UTF-16 units).

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Repr {
    Text(String),
    Bytes { bytes: Vec<u8> },
    Wide { wide: Vec<u16> },
}

fn to_repr(path: &Path) -> Repr {
    if let Some(text) = path.to_str() {
        return Repr::Text(text.to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Repr::Bytes {
            bytes: path.as_os_str().as_bytes().to_vec(),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        Repr::Wide {
            wide: path.as_os_str().encode_wide().collect(),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Repr::Text(path.to_string_lossy().into_owned())
    }
}

fn from_repr(repr: Repr) -> Result<PathBuf, String> {
    match repr {
        Repr::Text(text) => Ok(PathBuf::from(text)),
        #[cfg(unix)]
        Repr::Bytes { bytes } => {
            use std::os::unix::ffi::OsStringExt;
            Ok(PathBuf::from(OsString::from_vec(bytes)))
        }
        #[cfg(windows)]
        Repr::Wide { wide } => {
            use std::os::windows::ffi::OsStringExt;
            Ok(PathBuf::from(OsString::from_wide(&wide)))
        }
        #[allow(unreachable_patterns)]
        _ => Err("a path stored by another platform can't be read here".to_string()),
    }
}

pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    to_repr(path).serialize(serializer)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    from_repr(Repr::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}

/// The same for `Option<PathBuf>`.
pub mod option {
    use super::*;

    pub fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        path.as_deref().map(to_repr).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Option::<Repr>::deserialize(deserializer)?
            .map(from_repr)
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Holder {
        #[serde(with = "super")]
        path: PathBuf,
        #[serde(with = "super::option", default)]
        maybe: Option<PathBuf>,
    }

    #[test]
    fn utf8_paths_stay_plain_strings() {
        let holder = Holder {
            path: PathBuf::from("/data/café"),
            maybe: None,
        };
        let json = serde_json::to_string(&holder).unwrap();
        assert_eq!(json, r#"{"path":"/data/café","maybe":null}"#);
        assert_eq!(serde_json::from_str::<Holder>(&json).unwrap(), holder);
        // A manifest without the optional field still loads.
        assert!(serde_json::from_str::<Holder>(r#"{"path":"/x"}"#).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_round_trip() {
        use std::os::unix::ffi::OsStringExt;
        let weird = PathBuf::from(OsString::from_vec(b"/tmp/caf\xE9-\xFF".to_vec()));
        let holder = Holder {
            path: weird.clone(),
            maybe: Some(weird),
        };
        let json = serde_json::to_string(&holder).unwrap();
        assert!(json.contains("bytes"), "{json}");
        assert_eq!(serde_json::from_str::<Holder>(&json).unwrap(), holder);
    }
}
