//! Semver parsing and comparison for release tags.

use std::{cmp::Ordering, fmt, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreIdent {
   Numeric(u64),
   Alpha(String),
}

impl Ord for PreIdent {
   fn cmp(&self, other: &Self) -> Ordering {
      match (self, other) {
         (Self::Numeric(a), Self::Numeric(b)) => a.cmp(b),
         (Self::Alpha(a), Self::Alpha(b)) => a.cmp(b),
         (Self::Numeric(_), Self::Alpha(_)) => Ordering::Less,
         (Self::Alpha(_), Self::Numeric(_)) => Ordering::Greater,
      }
   }
}

impl PartialOrd for PreIdent {
   fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
      Some(self.cmp(other))
   }
}

impl fmt::Display for PreIdent {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      match self {
         Self::Numeric(n) => write!(f, "{n}"),
         Self::Alpha(s) => f.write_str(s),
      }
   }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
   pub major: u64,
   pub minor: u64,
   pub patch: u64,
   pub prerelease: Vec<PreIdent>,
}

impl Version {
   #[must_use]
   pub fn parse(raw: &str) -> Option<Self> {
      let trimmed = raw.trim();
      let stripped = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
      if stripped.is_empty() {
         return None;
      }

      let without_build = match stripped.split_once('+') {
         Some((head, build)) => {
            if !is_valid_meta(build) {
               return None;
            }
            head
         },
         None => stripped,
      };

      let (core, pre_raw) = match without_build.split_once('-') {
         Some((head, pre)) => (head, Some(pre)),
         None => (without_build, None),
      };

      let mut parts = core.split('.');
      let major = parse_digits(parts.next()?)?;
      let minor = parse_digits(parts.next()?)?;
      let patch = parse_digits(parts.next()?)?;
      if parts.next().is_some() {
         return None;
      }

      let mut prerelease = Vec::new();
      if let Some(pre) = pre_raw {
         if pre.is_empty() {
            return None;
         }
         for ident in pre.split('.') {
            if ident.is_empty()
               || !ident
                  .bytes()
                  .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
               return None;
            }
            if let Some(num) = parse_digits(ident) {
               prerelease.push(PreIdent::Numeric(num));
            } else {
               prerelease.push(PreIdent::Alpha(ident.to_owned()));
            }
         }
      }

      Some(Self {
         major,
         minor,
         patch,
         prerelease,
      })
   }
}

impl Ord for Version {
   fn cmp(&self, other: &Self) -> Ordering {
      self
         .major
         .cmp(&other.major)
         .then_with(|| self.minor.cmp(&other.minor))
         .then_with(|| self.patch.cmp(&other.patch))
         .then_with(|| compare_prerelease(&self.prerelease, &other.prerelease))
   }
}

impl PartialOrd for Version {
   fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
      Some(self.cmp(other))
   }
}

impl fmt::Display for Version {
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
      write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
      for (idx, ident) in self.prerelease.iter().enumerate() {
         if idx == 0 {
            f.write_str("-")?;
         } else {
            f.write_str(".")?;
         }
         write!(f, "{ident}")?;
      }
      Ok(())
   }
}

impl FromStr for Version {
   type Err = ();

   fn from_str(s: &str) -> Result<Self, Self::Err> {
      Self::parse(s).ok_or(())
   }
}

fn parse_digits(part: &str) -> Option<u64> {
   if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
      return None;
   }
   part.parse::<u64>().ok()
}

fn is_valid_meta(meta: &str) -> bool {
   !meta.is_empty()
      && meta.split('.').all(|part| {
         !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
      })
}

fn compare_prerelease(a: &[PreIdent], b: &[PreIdent]) -> Ordering {
   match (a.is_empty(), b.is_empty()) {
      (true, true) => Ordering::Equal,
      (true, false) => Ordering::Greater,
      (false, true) => Ordering::Less,
      (false, false) => {
         for (left, right) in a.iter().zip(b.iter()) {
            let ord = left.cmp(right);
            if ord != Ordering::Equal {
               return ord;
            }
         }
         a.len().cmp(&b.len())
      },
   }
}

/// Parses a release version tag.
#[must_use]
pub fn parse(raw: &str) -> Option<Version> {
   Version::parse(raw)
}

/// Returns true when `candidate` is newer than `current`.
///
/// Legacy or dev strings that do not parse as 3 part semver are replaced by any
/// valid semver release.
#[must_use]
pub fn is_newer(candidate: &str, current: &str) -> bool {
   let Some(cand) = Version::parse(candidate) else {
      return false;
   };
   Version::parse(current).is_none_or(|cur| cand > cur)
}

/// Strips a leading `v` or `V` and surrounding whitespace.
#[must_use]
pub fn normalize(v: &str) -> String {
   let trimmed = v.trim();
   trimmed
      .strip_prefix(['v', 'V'])
      .unwrap_or(trimmed)
      .to_owned()
}

#[cfg(test)]
mod tests {
   use super::*;

   #[test]
   fn parses_and_normalizes_tags() {
      let v = parse("v0.1.10").expect("valid semver");
      assert_eq!(v.major, 0);
      assert_eq!(v.minor, 1);
      assert_eq!(v.patch, 10);
      assert_eq!(v.prerelease, Vec::<PreIdent>::new());
      assert_eq!(v.to_string(), "0.1.10");

      let pre = parse("V0.0.9-rc.1").expect("valid prerelease");
      assert_eq!(pre.to_string(), "0.0.9-rc.1");

      assert_eq!(normalize("v1.2.3"), "1.2.3");
      assert_eq!(normalize("V1.2.3"), "1.2.3");
      assert_eq!(normalize(" 1.2.3 "), "1.2.3");
   }

   #[test]
   fn rejects_non_semver_inputs() {
      for bad in [
         "",
         "0",
         "1.2",
         "1.2.3.4",
         "2026.11.26.0843-7f5c2ee",
         "v1.2.3-",
         "v1.2.3-rc..1",
         "v1.2.a",
      ] {
         assert!(parse(bad).is_none(), "expected {bad:?} to fail parsing");
      }
   }

   #[test]
   fn compares_semver_and_prereleases() {
      assert!(is_newer("v0.1.10", "0.1.2"));
      assert!(!is_newer("0.1.2", "v0.1.10"));
      assert!(!is_newer("v0.1.0", "0.1.0"));
      assert!(is_newer("0.1.0", "0.1.0-rc.1"));
      assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
      assert!(is_newer("0.1.0-rc.10", "0.1.0-rc.2"));
      assert!(is_newer("0.1.0-beta.1", "0.1.0-alpha.9"));
      assert!(is_newer("0.1.0-alpha.1", "0.1.0-1"));
      assert!(is_newer("0.1.0-rc.1.1", "0.1.0-rc.1"));
   }

   #[test]
   fn replaces_legacy_and_dev_versions() {
      assert!(is_newer("v0.1.0", "0"));
      assert!(is_newer("v0.1.0", ""));
      assert!(is_newer("v0.1.0", "2026.11.26.0843-7f5c2ee"));
      assert!(!is_newer("not-a-version", "0"));
      assert!(!is_newer("2026.11.26.0843-7f5c2ee", "0"));
   }
}
