//! The journal: validated responses for one relic in progress, one compressed file per block.
//!
//! Mirror-local and never published. The directory listing is the truth about what is held, so a
//! crash between write and rename leaves only a temporary file that the next open removes.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use legacy_format::relic::BlockRange;

use super::wire::Journaled;
use super::Error;

const SUFFIX: &str = ".json.zst";
const TEMP: &str = ".tmp";
// Durability is the journal's job, so the binary always syncs. Unit tests seal whole fixture relics
// many times over and test resumption by restarting, not by pulling the power, so they skip it.
const FSYNC: bool = !cfg!(test);

pub struct Journal {
    blocks: PathBuf,
    root: PathBuf,
}

impl Journal {
    pub fn open(root: &Path) -> Result<Self, Error> {
        let blocks = root.join("blocks");
        std::fs::create_dir_all(&blocks)?;
        for entry in std::fs::read_dir(&blocks)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == &TEMP[1..]) {
                std::fs::remove_file(path)?;
            }
        }
        Ok(Self {
            blocks,
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, number: u64) -> PathBuf {
        self.blocks.join(format!("{number}{SUFFIX}"))
    }

    /// Block numbers held inside `range`. Files outside it are ignored, not trusted or deleted.
    pub fn held(&self, range: BlockRange) -> Result<BTreeSet<u64>, Error> {
        let mut held = BTreeSet::new();
        for entry in std::fs::read_dir(&self.blocks)? {
            let name = entry?.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(SUFFIX)) else {
                continue;
            };
            if let Ok(number) = stem.parse::<u64>() {
                if number >= range.start && number <= range.end {
                    held.insert(number);
                }
            }
        }
        Ok(held)
    }

    pub fn missing(&self, range: BlockRange) -> Result<Vec<u64>, Error> {
        let held = self.held(range)?;
        Ok((range.start..=range.end)
            .filter(|n| !held.contains(n))
            .collect())
    }

    /// Write atomically: temp file, fsync, rename. Only validated responses reach this.
    pub fn put(&self, number: u64, block: &Journaled) -> Result<(), Error> {
        let json = serde_json::to_vec(block)?;
        let compressed = zstd::bulk::compress(&json, 3)?;
        let temp = self.blocks.join(format!("{number}{SUFFIX}{TEMP}"));
        {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&compressed)?;
            if FSYNC {
                file.sync_all()?;
            }
        }
        std::fs::rename(&temp, self.path(number))?;
        Ok(())
    }

    pub fn get(&self, number: u64) -> Result<Journaled, Error> {
        let path = self.path(number);
        let compressed = std::fs::read(&path)?;
        let mut json = Vec::new();
        zstd::stream::copy_decode(compressed.as_slice(), &mut json)?;
        serde_json::from_slice(&json).map_err(|e| {
            Error::Journal(format!(
                "{} is not a journaled block: {e}; delete it and resume",
                path.display()
            ))
        })
    }

    pub fn write_provenance(&self, provenance: &serde_json::Value) -> Result<(), Error> {
        let bytes = serde_json::to_vec_pretty(provenance)?;
        std::fs::write(self.root.join("provenance.json"), bytes)?;
        Ok(())
    }
}
