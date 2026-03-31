use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Item – a single segment inside a parsed version
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Item {
    Int(u64),
    Str(StringItem),
    List(Vec<Item>),
}

#[derive(Debug, Clone)]
struct StringItem {
    value: String, // lowercase, canonical
}

/// Returns the rank for well-known qualifiers.
/// Unknown qualifiers get a rank that sorts after "sp".
fn qualifier_rank(s: &str) -> Option<u32> {
    match s {
        "alpha" | "a" => Some(0),
        "beta" | "b" => Some(1),
        "milestone" | "m" => Some(2),
        "rc" | "cr" => Some(3),
        "snapshot" => Some(4),
        "" => Some(5),
        "sp" => Some(6),
        _ => None,
    }
}

impl StringItem {
    fn new(s: &str) -> Self {
        Self {
            value: s.to_ascii_lowercase(),
        }
    }

    fn rank(&self) -> Option<u32> {
        qualifier_rank(&self.value)
    }
}

impl PartialEq for StringItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for StringItem {}

impl PartialOrd for StringItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StringItem {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.rank(), other.rank()) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => self.value.cmp(&other.value),
        }
    }
}

// Comparison helpers for Item

impl Item {
    /// The "zero" value used when comparing lists of different length.
    fn is_null(&self) -> bool {
        match self {
            Item::Int(n) => *n == 0,
            Item::Str(s) => s.value.is_empty(),
            Item::List(l) => l.iter().all(Item::is_null),
        }
    }
}

fn compare_item(a: &Item, b: &Item) -> Ordering {
    match (a, b) {
        (Item::Int(x), Item::Int(y)) => x.cmp(y),
        (Item::Int(_), Item::Str(_)) => {
            // numeric > string qualifier at same position
            Ordering::Greater
        }
        (Item::Str(_), Item::Int(_)) => Ordering::Less,
        (Item::Str(x), Item::Str(y)) => x.cmp(y),
        (Item::List(la), Item::List(lb)) => compare_item_lists(la, lb),
        // List vs atomic: wrap atomic in a single-element list
        (Item::List(la), other) => compare_item_lists(la, &[other.clone()]),
        (other, Item::List(lb)) => compare_item_lists(&[other.clone()], lb),
    }
}

fn compare_item_lists(a: &[Item], b: &[Item]) -> Ordering {
    let len = a.len().max(b.len());
    for i in 0..len {
        let ia = a.get(i);
        let ib = b.get(i);
        let ord = match (ia, ib) {
            (Some(x), Some(y)) => compare_item(x, y),
            (Some(x), None) => compare_against_null(x),
            (None, Some(y)) => compare_against_null(y).reverse(),
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Compare an item against the implicit "null" (0 / "" / empty list).
fn compare_against_null(item: &Item) -> Ordering {
    match item {
        Item::Int(n) => n.cmp(&0),
        Item::Str(s) => {
            // release-qualifier ("") == null
            let rank = s.rank().unwrap_or(7);
            rank.cmp(&5) // 5 == release
        }
        Item::List(items) => {
            for it in items {
                let c = compare_against_null(it);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing (ported from Maven's ComparableVersion.java)
// ---------------------------------------------------------------------------

fn parse_items(version: &str) -> Vec<Item> {
    let v = version.trim();
    if v.is_empty() {
        return vec![];
    }

    // We maintain a stack of lists. The top of the stack is the current list
    // we are appending items to. A '-' pushes a new sub-list.
    let mut stack: Vec<Vec<Item>> = vec![vec![]];

    let mut chars = v.chars().peekable();
    let mut buf = String::new();
    let mut is_digit = false;

    while let Some(&ch) = chars.peek() {
        if ch == '.' || ch == '-' {
            chars.next();
            // flush buffer
            flush_buf(&mut buf, is_digit, &mut stack);
            buf.clear();

            if ch == '-' {
                // start a new sub-list
                let sub: Vec<Item> = vec![];
                stack.push(sub);
            }
        } else if ch.is_ascii_digit() {
            if !buf.is_empty() && !is_digit {
                // transition string→digit ⇒ implicit '-'
                flush_buf(&mut buf, false, &mut stack);
                buf.clear();
                stack.push(vec![]);
            }
            is_digit = true;
            buf.push(ch);
            chars.next();
        } else {
            if !buf.is_empty() && is_digit {
                // transition digit→string ⇒ implicit '-'
                flush_buf(&mut buf, true, &mut stack);
                buf.clear();
                stack.push(vec![]);
            }
            is_digit = false;
            buf.push(ch);
            chars.next();
        }
    }

    // flush last buffer
    flush_buf(&mut buf, is_digit, &mut stack);

    // collapse the stack
    while stack.len() > 1 {
        let top = stack.pop().unwrap();
        let list_item = Item::List(trim_trailing_nulls(top));
        stack.last_mut().unwrap().push(list_item);
    }

    trim_trailing_nulls(stack.pop().unwrap())
}

fn flush_buf(buf: &mut String, is_digit: bool, stack: &mut Vec<Vec<Item>>) {
    if buf.is_empty() {
        // Still push a null marker for empty segments (e.g. "1..2")
        // Maven treats missing segment as release qualifier
        stack.last_mut().unwrap().push(Item::Str(StringItem::new("")));
        return;
    }
    let item = if is_digit {
        let n = buf.parse::<u64>().unwrap_or(0);
        Item::Int(n)
    } else {
        Item::Str(StringItem::new(buf))
    };
    stack.last_mut().unwrap().push(item);
}

fn trim_trailing_nulls(mut items: Vec<Item>) -> Vec<Item> {
    while items.last().map_or(false, |it| it.is_null()) {
        items.pop();
    }
    items
}

// ---------------------------------------------------------------------------
// Version
// ---------------------------------------------------------------------------

/// A Maven version, parsed and comparable according to Maven's rules.
#[derive(Clone, Debug)]
pub struct Version {
    original: String,
    items: Vec<Item>,
}

impl Version {
    pub fn new(s: &str) -> Self {
        Version {
            original: s.to_string(),
            items: parse_items(s),
        }
    }

    /// Returns the original string representation.
    pub fn as_str(&self) -> &str {
        &self.original
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.original)
    }
}

impl FromStr for Version {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Version::new(s))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_item_lists(&self.items, &other.items)
    }
}

impl Hash for Version {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash the canonical parsed items so that versions that compare
        // equal (e.g. "1" and "1.0") produce the same hash.
        self.items.hash(state);
    }
}

impl Hash for Item {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Item::Int(n) => n.hash(state),
            Item::Str(s) => s.hash(state),
            Item::List(items) => items.hash(state),
        }
    }
}

impl Hash for StringItem {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash the rank for well-known qualifiers so aliases hash the same.
        // Unknown qualifiers hash by their lowercase string value.
        match self.rank() {
            Some(r) => r.hash(state),
            None => self.value.hash(state),
        }
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.original)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Version::new(&s))
    }
}

// ---------------------------------------------------------------------------
// VersionRange – a single interval bound
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Bound {
    version: Option<Version>,
    inclusive: bool,
}

/// A contiguous version range such as `[1.0, 2.0)`.
#[derive(Debug, Clone)]
struct Interval {
    lower: Bound,
    upper: Bound,
}

impl Interval {
    fn contains(&self, v: &Version) -> bool {
        // check lower
        if let Some(ref lo) = self.lower.version {
            let cmp = v.cmp(lo);
            if self.lower.inclusive {
                if cmp == Ordering::Less {
                    return false;
                }
            } else if cmp != Ordering::Greater {
                return false;
            }
        }
        // check upper
        if let Some(ref hi) = self.upper.version {
            let cmp = v.cmp(hi);
            if self.upper.inclusive {
                if cmp == Ordering::Greater {
                    return false;
                }
            } else if cmp != Ordering::Less {
                return false;
            }
        }
        true
    }
}

/// A Maven version range expression, possibly a union of intervals.
///
/// Examples: `[1.0,2.0)`, `(,1.0],[1.2,)`, `[1.0]`
#[derive(Debug, Clone)]
pub struct VersionRange {
    intervals: Vec<Interval>,
}

impl VersionRange {
    /// Parse a range expression (the part that uses brackets).
    fn parse_range(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty range expression".into());
        }

        let mut intervals = Vec::new();
        let mut rest = s;

        while !rest.is_empty() {
            // skip leading comma between ranges
            rest = rest.trim_start_matches(',').trim();
            if rest.is_empty() {
                break;
            }

            let (interval, remaining) = parse_single_interval(rest)?;
            intervals.push(interval);
            rest = remaining.trim();
        }

        if intervals.is_empty() {
            return Err(format!("no intervals found in '{s}'"));
        }

        Ok(VersionRange { intervals })
    }

    /// Does this range contain the given version?
    pub fn contains(&self, v: &Version) -> bool {
        self.intervals.iter().any(|iv| iv.contains(v))
    }
}

fn parse_single_interval(s: &str) -> Result<(Interval, &str), String> {
    let s = s.trim();
    let open_bracket = s.as_bytes().first().copied();
    let lower_inclusive = match open_bracket {
        Some(b'[') => true,
        Some(b'(') => false,
        _ => return Err(format!("expected '[' or '(' at start of interval, got: {s}")),
    };

    // find matching close bracket
    let close_pos = s
        .find(']')
        .or_else(|| s.find(')'));
    let close_pos = match close_pos {
        Some(p) => p,
        None => return Err(format!("missing closing bracket in '{s}'")),
    };

    let upper_inclusive = s.as_bytes()[close_pos] == b']';
    let inner = &s[1..close_pos];
    let remaining = &s[close_pos + 1..];

    // split inner on ','
    if let Some(comma_pos) = inner.find(',') {
        let lo_str = inner[..comma_pos].trim();
        let hi_str = inner[comma_pos + 1..].trim();

        let lower_version = if lo_str.is_empty() {
            None
        } else {
            Some(Version::new(lo_str))
        };
        let upper_version = if hi_str.is_empty() {
            None
        } else {
            Some(Version::new(hi_str))
        };

        Ok((
            Interval {
                lower: Bound {
                    version: lower_version,
                    inclusive: lower_inclusive,
                },
                upper: Bound {
                    version: upper_version,
                    inclusive: upper_inclusive,
                },
            },
            remaining,
        ))
    } else {
        // exact version: [1.0]
        let v = Version::new(inner.trim());
        Ok((
            Interval {
                lower: Bound {
                    version: Some(v.clone()),
                    inclusive: true,
                },
                upper: Bound {
                    version: Some(v),
                    inclusive: true,
                },
            },
            remaining,
        ))
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, iv) in self.intervals.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            if iv.lower.inclusive {
                write!(f, "[")?;
            } else {
                write!(f, "(")?;
            }
            // check exact match shorthand
            let is_exact = iv.lower.inclusive
                && iv.upper.inclusive
                && iv.lower.version.is_some()
                && iv.upper.version.is_some()
                && iv.lower.version.as_ref().unwrap() == iv.upper.version.as_ref().unwrap();
            if is_exact {
                write!(f, "{}", iv.lower.version.as_ref().unwrap())?;
            } else {
                if let Some(ref lo) = iv.lower.version {
                    write!(f, "{lo}")?;
                }
                write!(f, ",")?;
                if let Some(ref hi) = iv.upper.version {
                    write!(f, "{hi}")?;
                }
            }
            if iv.upper.inclusive {
                write!(f, "]")?;
            } else {
                write!(f, ")")?;
            }
        }
        Ok(())
    }
}

impl FromStr for VersionRange {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        VersionRange::parse_range(s)
    }
}

// ---------------------------------------------------------------------------
// VersionConstraint
// ---------------------------------------------------------------------------

/// A version constraint: either a soft recommended version or a hard range.
#[derive(Debug, Clone)]
pub enum VersionConstraint {
    /// A soft/recommended version (e.g., `"1.0"`). `contains` always returns true.
    Recommended(Version),
    /// A hard range constraint (e.g., `"[1.0,2.0)"`).
    Range(VersionRange),
}

impl VersionConstraint {
    /// Parse a version constraint string.
    ///
    /// If the string starts with `[` or `(`, it is parsed as a range.
    /// Otherwise, it is treated as a recommended version.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty version constraint".into());
        }
        let first = s.as_bytes()[0];
        if first == b'[' || first == b'(' {
            let range = VersionRange::parse_range(s)?;
            Ok(VersionConstraint::Range(range))
        } else {
            Ok(VersionConstraint::Recommended(Version::new(s)))
        }
    }

    /// Check if a version satisfies this constraint.
    ///
    /// Recommended versions always return `true` (soft requirement).
    /// Range constraints check actual containment.
    pub fn contains(&self, v: &Version) -> bool {
        match self {
            VersionConstraint::Recommended(_) => true,
            VersionConstraint::Range(r) => r.contains(v),
        }
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionConstraint::Recommended(v) => write!(f, "{v}"),
            VersionConstraint::Range(r) => write!(f, "{r}"),
        }
    }
}

impl FromStr for VersionConstraint {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        VersionConstraint::parse(s)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::new(s)
    }

    // ---- equality / trailing zeros ----

    #[test]
    fn test_version_equality_trailing_zeros() {
        assert_eq!(v("1"), v("1.0"));
        assert_eq!(v("1"), v("1.0.0"));
        assert_eq!(v("1.0"), v("1.0.0"));
    }

    #[test]
    fn test_case_insensitive() {
        assert_eq!(v("1.0-ALPHA"), v("1.0-alpha"));
        assert_eq!(v("1.0-Beta"), v("1.0-beta"));
        assert_eq!(v("1.0-SNAPSHOT"), v("1.0-snapshot"));
    }

    // ---- basic ordering ----

    #[test]
    fn test_major_minor_ordering() {
        assert!(v("1") < v("1.1"));
        assert!(v("1.1") < v("1.2"));
        assert!(v("1.2") < v("2"));
    }

    // ---- qualifier ordering ----

    #[test]
    fn test_qualifier_ordering() {
        assert!(v("1.0-alpha") < v("1.0-beta"));
        assert!(v("1.0-beta") < v("1.0-milestone"));
        assert!(v("1.0-milestone") < v("1.0-rc"));
        assert!(v("1.0-rc") < v("1.0-SNAPSHOT"));
        assert!(v("1.0-SNAPSHOT") < v("1.0"));
        assert!(v("1.0") < v("1.0-sp"));
        assert!(v("1.0-sp") < v("1.0.1"));
    }

    #[test]
    fn test_alpha_beta_sub_versions() {
        assert!(v("1.0-alpha-1") < v("1.0-alpha-2"));
        assert!(v("1.0-alpha-2") < v("1.0-beta-1"));
    }

    #[test]
    fn test_snapshot_less_than_release() {
        assert!(v("1.0-SNAPSHOT") < v("1.0"));
    }

    #[test]
    fn test_numeric_after_dash() {
        assert!(v("1.0.0-1") < v("1.0.0-2"));
    }

    // ---- version range ----

    #[test]
    fn test_range_closed_open() {
        let range: VersionRange = "[1.0,2.0)".parse().unwrap();
        assert!(range.contains(&v("1.0")));
        assert!(range.contains(&v("1.5")));
        assert!(!range.contains(&v("2.0")));
        assert!(!range.contains(&v("0.9")));
    }

    #[test]
    fn test_range_exact() {
        let range: VersionRange = "[1.0]".parse().unwrap();
        assert!(range.contains(&v("1.0")));
        assert!(range.contains(&v("1.0.0"))); // 1.0 == 1.0.0
        assert!(!range.contains(&v("1.1")));
        assert!(!range.contains(&v("0.9")));
    }

    #[test]
    fn test_range_lower_unbounded() {
        let range: VersionRange = "(,1.0]".parse().unwrap();
        assert!(range.contains(&v("0.5")));
        assert!(range.contains(&v("1.0")));
        assert!(!range.contains(&v("1.1")));
    }

    #[test]
    fn test_range_upper_unbounded() {
        let range: VersionRange = "[1.5,)".parse().unwrap();
        assert!(range.contains(&v("1.5")));
        assert!(range.contains(&v("2.0")));
        assert!(!range.contains(&v("1.4")));
    }

    #[test]
    fn test_range_multi() {
        let range: VersionRange = "(,1.0],[1.2,)".parse().unwrap();
        assert!(range.contains(&v("0.5")));
        assert!(range.contains(&v("1.0")));
        assert!(!range.contains(&v("1.1")));
        assert!(range.contains(&v("1.2")));
        assert!(range.contains(&v("2.0")));
    }

    #[test]
    fn test_range_closed_closed() {
        let range: VersionRange = "[1.0,2.0]".parse().unwrap();
        assert!(range.contains(&v("1.0")));
        assert!(range.contains(&v("2.0")));
        assert!(!range.contains(&v("2.1")));
    }

    // ---- version constraint ----

    #[test]
    fn test_constraint_recommended() {
        let c = VersionConstraint::parse("1.0").unwrap();
        assert!(matches!(c, VersionConstraint::Recommended(_)));
        assert!(c.contains(&v("999.0"))); // always true
    }

    #[test]
    fn test_constraint_range() {
        let c = VersionConstraint::parse("[1.0,2.0)").unwrap();
        assert!(matches!(c, VersionConstraint::Range(_)));
        assert!(c.contains(&v("1.5")));
        assert!(!c.contains(&v("2.0")));
    }

    // ---- display / from_str round-trip ----

    #[test]
    fn test_version_display() {
        let ver = v("1.2.3-beta-1");
        assert_eq!(ver.to_string(), "1.2.3-beta-1");
    }

    #[test]
    fn test_version_from_str() {
        let ver: Version = "2.0.0".parse().unwrap();
        assert_eq!(ver, v("2.0.0"));
    }

    #[test]
    fn test_version_range_display() {
        let r: VersionRange = "[1.0,2.0)".parse().unwrap();
        assert_eq!(r.to_string(), "[1.0,2.0)");
    }

    // ---- serde ----

    #[test]
    fn test_serde_roundtrip() {
        let ver = v("3.1.0-SNAPSHOT");
        let json = serde_json::to_string(&ver).unwrap();
        assert_eq!(json, "\"3.1.0-SNAPSHOT\"");
        let back: Version = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ver);
    }

    // ==== Edge-case tests ====

    #[test]
    fn version_empty_string() {
        let ver: Version = "".parse().unwrap();
        assert_eq!(ver.to_string(), "");
    }

    #[test]
    fn version_single_number() {
        assert_eq!(v("42"), v("42.0"));
    }

    #[test]
    fn version_very_long() {
        let ver = v("1.2.3.4.5.6.7.8.9.10.11.12");
        assert!(ver < v("1.2.3.4.5.6.7.8.9.10.11.13"));
        assert!(ver > v("1.2.3.4.5.6.7.8.9.10.11.11"));
        assert_eq!(ver, v("1.2.3.4.5.6.7.8.9.10.11.12"));
    }

    #[test]
    fn version_all_qualifiers_ordering() {
        let chain = [
            "1-alpha", "1-beta", "1-milestone", "1-rc", "1-snapshot", "1", "1-sp", "1.1",
        ];
        for pair in chain.windows(2) {
            assert!(
                v(pair[0]) < v(pair[1]),
                "expected {} < {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn version_qualifier_aliases() {
        assert_eq!(v("1-a1"), v("1-alpha-1"));
        assert_eq!(v("1-b2"), v("1-beta-2"));
        assert_eq!(v("1-m3"), v("1-milestone-3"));
        assert_eq!(v("1-cr1"), v("1-rc-1"));
    }

    #[test]
    fn version_numeric_after_dash() {
        assert!(v("1.0-1") < v("1.0-2"));
        assert!(v("1.0-2") < v("1.0-11"));
    }

    #[test]
    fn version_mixed_separators() {
        assert!(v("1.0-alpha.1") < v("1.0-alpha.2"));
    }

    #[test]
    fn version_zero_padding() {
        assert_eq!(v("1.0"), v("1.0.0.0.0"));
    }

    #[test]
    fn version_display_preserves_original() {
        let ver = Version::new("1.0-ALPHA");
        assert_eq!(ver.to_string(), "1.0-ALPHA");
    }

    #[test]
    fn version_serde_roundtrip() {
        let ver = v("2.3.1-beta-4");
        let json = serde_json::to_string(&ver).unwrap();
        let back: Version = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ver);
        assert_eq!(back.to_string(), "2.3.1-beta-4");
    }

    #[test]
    fn version_hash_equality() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(v("1.0"));
        // "1" == "1.0", so inserting "1" should not grow the set
        set.insert(v("1"));
        assert_eq!(set.len(), 1, "equal versions must have the same hash");
    }

    // ---- range edge cases ----

    #[test]
    fn range_exactly_one() {
        let range: VersionRange = "[1.5]".parse().unwrap();
        assert!(range.contains(&v("1.5")));
        assert!(!range.contains(&v("1.4")));
        assert!(!range.contains(&v("1.6")));
    }

    #[test]
    fn range_open_ended_right() {
        let range: VersionRange = "[2.0,)".parse().unwrap();
        assert!(range.contains(&v("2.0")));
        assert!(range.contains(&v("3.0")));
        assert!(range.contains(&v("999.0")));
        assert!(!range.contains(&v("1.9")));
    }

    #[test]
    fn range_open_ended_left() {
        let range: VersionRange = "(,1.0]".parse().unwrap();
        assert!(range.contains(&v("0.1")));
        assert!(range.contains(&v("1.0")));
        assert!(!range.contains(&v("1.1")));
    }

    #[test]
    fn range_exclusive_bounds() {
        let range: VersionRange = "(1.0,2.0)".parse().unwrap();
        assert!(!range.contains(&v("1.0")));
        assert!(!range.contains(&v("2.0")));
        assert!(range.contains(&v("1.5")));
    }

    #[test]
    fn range_multi_range() {
        let range: VersionRange = "(,1.0],[2.0,)".parse().unwrap();
        assert!(range.contains(&v("0.5")));
        assert!(range.contains(&v("1.0")));
        assert!(range.contains(&v("2.0")));
        assert!(range.contains(&v("3.0")));
        assert!(!range.contains(&v("1.5")));
    }

    #[test]
    fn range_display_roundtrip() {
        let input = "(,1.0],[2.0,)";
        let r1: VersionRange = input.parse().unwrap();
        let displayed = r1.to_string();
        let r2: VersionRange = displayed.parse().unwrap();
        // verify they agree on containment
        for ver in &["0.5", "1.0", "1.5", "2.0", "3.0"] {
            assert_eq!(
                r1.contains(&v(ver)),
                r2.contains(&v(ver)),
                "mismatch for {ver}"
            );
        }
    }

    // ---- VersionConstraint edge cases ----

    #[test]
    fn constraint_recommended_always_matches() {
        let c = VersionConstraint::parse("1.0").unwrap();
        assert!(c.contains(&v("0.1")));
        assert!(c.contains(&v("1.0")));
        assert!(c.contains(&v("999.0")));
    }

    #[test]
    fn constraint_range_strict() {
        let c = VersionConstraint::parse("[1.0,2.0)").unwrap();
        assert!(c.contains(&v("1.0")));
        assert!(c.contains(&v("1.5")));
        assert!(!c.contains(&v("2.0")));
        assert!(!c.contains(&v("0.9")));
    }

    #[test]
    fn constraint_parse_distinguishes() {
        let rec = VersionConstraint::parse("1.0").unwrap();
        assert!(matches!(rec, VersionConstraint::Recommended(_)));

        let rng = VersionConstraint::parse("[1.0,2.0)").unwrap();
        assert!(matches!(rng, VersionConstraint::Range(_)));
    }
}
