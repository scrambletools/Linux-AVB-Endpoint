//! Entity models: the identity an app gives the entities it owns in AEM,
//! from a file its package installs, `<model>.toml` in the models folder,
//! `avb-endpointd/models` under the data folders:
//! `$XDG_DATA_HOME` (`~/.local/share`), then each of `$XDG_DATA_DIRS`
//! (`/usr/local/share` and `/usr/share`).
//!
//! ```toml
//! # /usr/share/avb-endpointd/models/plaa.toml
//! vendor_name = "Scramble Tools"
//! model_name = "plaa"
//! firmware_version = "0.6.0"
//! logo = "plaa.png"
//! vendor_logo = "scramble-vendor.png"
//! ```
//!
//! Logos are PNG files, square, 128 pixels a side, of at most
//! [`MOST_LOGO`] bytes, named from the model file's folder. A model file
//! gives no entity model ID: the daemon makes one from the entity's
//! streams, the model's names and its logos, leaving the firmware version
//! out, as controllers read that again at each enumeration.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::{self, LoadError};

/// A model file as it is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub vendor_name: String,
    pub model_name: String,
    /// The app's version, as its package writes it; the daemon's when
    /// left out.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub firmware_version: String,
    /// The entity's logo and its vendor's, from the model file's folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_logo: Option<PathBuf>,
}

/// The largest logo a model may give, in bytes.
pub const MOST_LOGO: u64 = 64 * 1024;

/// Why a model could not be used.
#[derive(Debug)]
pub enum ModelError {
    /// Not a name a model file can have: letters, digits, dots, dashes
    /// and underscores, not starting with a dot.
    BadName,
    /// No models folder has its file.
    Missing,
    /// Its file, or a logo it names, could not be read or is not valid.
    Unusable(PathBuf, String),
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadName => write!(formatter, "not a model's name"),
            Self::Missing => write!(formatter, "no models folder has it"),
            Self::Unusable(path, why) => write!(formatter, "{}: {why}", path.display()),
        }
    }
}

impl std::error::Error for ModelError {}

/// Whether `name` can name a model file.
pub fn is_model_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The models folders, searched in order.
pub fn dirs() -> Vec<PathBuf> {
    let absolute = |path: PathBuf| path.is_absolute().then_some(path);
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .and_then(absolute)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .and_then(absolute)
                .map(|home| home.join(".local/share"))
        });
    let shared = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(shared.split(':').map(PathBuf::from).filter_map(absolute))
        .map(|dir| dir.join("avb-endpointd/models"))
        .collect()
}

/// An entity's identity as controllers see it: a model with its logos
/// read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub vendor_name: String,
    pub model_name: String,
    pub firmware_version: String,
    pub logo: Option<Vec<u8>>,
    pub vendor_logo: Option<Vec<u8>>,
}

impl Identity {
    /// The model `name` from the first models folder that has it.
    pub fn find(name: &str) -> Result<Identity, ModelError> {
        Self::find_in(&dirs(), name)
    }

    /// The model `name` from the first of `dirs` that has it.
    pub fn find_in(dirs: &[PathBuf], name: &str) -> Result<Identity, ModelError> {
        if !is_model_name(name) {
            return Err(ModelError::BadName);
        }
        let path = dirs
            .iter()
            .map(|dir| dir.join(format!("{name}.toml")))
            .find(|path| path.is_file())
            .ok_or(ModelError::Missing)?;
        Self::load(&path)
    }

    /// The model in the file at `path`.
    pub fn load(path: &Path) -> Result<Identity, ModelError> {
        let unusable = |why: String| ModelError::Unusable(path.to_owned(), why);
        let model: Model = store::load(path).map_err(|error| match error {
            LoadError::Io(error) => unusable(error.to_string()),
            LoadError::Parse(error) => unusable(error.to_string()),
        })?;
        let folder = path.parent().unwrap_or(Path::new("."));
        let logo = |file: &Option<PathBuf>| -> Result<Option<Vec<u8>>, ModelError> {
            let Some(file) = file else {
                return Ok(None);
            };
            let path = folder.join(file);
            let bytes = read_logo(&path)
                .map_err(|error| ModelError::Unusable(path.clone(), error.to_string()))?;
            Ok(Some(bytes))
        };
        Ok(Identity {
            logo: logo(&model.logo)?,
            vendor_logo: logo(&model.vendor_logo)?,
            vendor_name: model.vendor_name,
            model_name: model.model_name,
            firmware_version: model.firmware_version,
        })
    }
}

/// A logo's bytes: a PNG file of at most [`MOST_LOGO`] bytes.
fn read_logo(path: &Path) -> io::Result<Vec<u8>> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    let size = std::fs::metadata(path)?.len();
    if size > MOST_LOGO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{size} bytes, more than {MOST_LOGO}"),
        ));
    }
    let bytes = std::fs::read(path)?;
    if !bytes.starts_with(PNG) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a PNG file"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("avb-model-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const LOGO: &[u8] = b"\x89PNG\r\n\x1a\nlogo";

    /// A model comes from the first folder that has it, its logos read
    /// from beside it.
    #[test]
    fn a_model_comes_from_the_first_folder_with_it() {
        let dir = scratch("first");
        let (home, shared) = (dir.join("home"), dir.join("shared"));
        for folder in [&home, &shared] {
            std::fs::create_dir_all(folder).unwrap();
        }
        std::fs::write(
            shared.join("plaa.toml"),
            "vendor_name = \"Scramble Tools\"\nmodel_name = \"plaa\"\n\
             firmware_version = \"0.6.0\"\nlogo = \"plaa.png\"\n",
        )
        .unwrap();
        std::fs::write(shared.join("plaa.png"), LOGO).unwrap();
        let dirs = [home.clone(), shared.clone()];
        let found = Identity::find_in(&dirs, "plaa").expect("found");
        assert_eq!(
            (found.vendor_name.as_str(), found.model_name.as_str()),
            ("Scramble Tools", "plaa")
        );
        assert_eq!(found.firmware_version, "0.6.0");
        assert_eq!(found.logo.as_deref(), Some(LOGO));
        assert_eq!(found.vendor_logo, None);
        // One in the person's own folder comes first.
        std::fs::write(
            home.join("plaa.toml"),
            "vendor_name = \"Me\"\nmodel_name = \"mine\"\n",
        )
        .unwrap();
        assert_eq!(Identity::find_in(&dirs, "plaa").unwrap().model_name, "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The default entity's model, as avb's packages install it, loads
    /// with its vendor's logo; checked where the source tree has it.
    #[test]
    fn the_default_entitys_model_loads() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/linux/models/avb-endpointd.toml");
        if !path.exists() {
            return;
        }
        let identity = Identity::load(&path).expect("loads");
        assert_eq!(identity.model_name, "avb-endpointd");
        assert!(identity.vendor_logo.is_some() && identity.logo.is_none());
    }

    /// A model that is not there, or not a model's name, or a logo that
    /// is not a small PNG, says why.
    #[test]
    fn a_missing_or_bad_model_says_why() {
        let dir = scratch("bad");
        let dirs = [dir.clone()];
        assert!(matches!(
            Identity::find_in(&dirs, "none"),
            Err(ModelError::Missing)
        ));
        for name in ["", "../etc/passwd", ".hidden", "a/b"] {
            assert!(
                matches!(Identity::find_in(&dirs, name), Err(ModelError::BadName)),
                "{name}"
            );
        }
        std::fs::write(
            dir.join("big.toml"),
            "vendor_name = \"V\"\nmodel_name = \"M\"\nlogo = \"big.png\"\n",
        )
        .unwrap();
        let mut big = LOGO.to_vec();
        big.resize(MOST_LOGO as usize + 1, 0);
        std::fs::write(dir.join("big.png"), big).unwrap();
        assert!(matches!(
            Identity::find_in(&dirs, "big"),
            Err(ModelError::Unusable(..))
        ));
        std::fs::write(dir.join("big.png"), b"GIF89a").unwrap();
        assert!(matches!(
            Identity::find_in(&dirs, "big"),
            Err(ModelError::Unusable(..))
        ));
        std::fs::write(dir.join("broken.toml"), "vendor_name = ").unwrap();
        assert!(matches!(
            Identity::find_in(&dirs, "broken"),
            Err(ModelError::Unusable(..))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
