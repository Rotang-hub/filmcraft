//! Fork: an effect's parameters, shared copy-on-write.
//!
//! Every edit copies the sequence it changes, and every undo step keeps the copy from before. A
//! map of parameters is the bulk of that copy (a graphic with many layers holds thousands), so it
//! sits behind an [`Arc`]: copying an effect shares its parameters, and the first change through
//! [`DerefMut`] gives that one effect its own map. Reads and writes look the same as on the
//! [`BTreeMap`] inside; JSON is the map's JSON.

use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::keyframe::Param;

/// The parameters of an effect instance, by id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParamMap(Arc<BTreeMap<String, Param>>);

impl ParamMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// The map, for code that needs to own it.
    pub fn into_map(self) -> BTreeMap<String, Param> {
        Arc::try_unwrap(self.0).unwrap_or_else(|shared| (*shared).clone())
    }

    /// Whether `self` and `other` are the same shared map (a copy nobody changed yet).
    pub fn shares(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Deref for ParamMap {
    type Target = BTreeMap<String, Param>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ParamMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.0)
    }
}

impl From<BTreeMap<String, Param>> for ParamMap {
    fn from(m: BTreeMap<String, Param>) -> Self {
        Self(Arc::new(m))
    }
}

impl From<ParamMap> for BTreeMap<String, Param> {
    fn from(m: ParamMap) -> Self {
        m.into_map()
    }
}

impl FromIterator<(String, Param)> for ParamMap {
    fn from_iter<I: IntoIterator<Item = (String, Param)>>(iter: I) -> Self {
        Self(Arc::new(iter.into_iter().collect()))
    }
}

impl Extend<(String, Param)> for ParamMap {
    fn extend<I: IntoIterator<Item = (String, Param)>>(&mut self, iter: I) {
        self.deref_mut().extend(iter);
    }
}

impl PartialEq<BTreeMap<String, Param>> for ParamMap {
    fn eq(&self, other: &BTreeMap<String, Param>) -> bool {
        *self.0 == *other
    }
}

impl<'a> IntoIterator for &'a ParamMap {
    type Item = (&'a String, &'a Param);
    type IntoIter = std::collections::btree_map::Iter<'a, String, Param>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut ParamMap {
    type Item = (&'a String, &'a mut Param);
    type IntoIter = std::collections::btree_map::IterMut<'a, String, Param>;
    fn into_iter(self) -> Self::IntoIter {
        self.deref_mut().iter_mut()
    }
}

impl IntoIterator for ParamMap {
    type Item = (String, Param);
    type IntoIter = std::collections::btree_map::IntoIter<String, Param>;
    fn into_iter(self) -> Self::IntoIter {
        self.into_map().into_iter()
    }
}

impl Serialize for ParamMap {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for ParamMap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        BTreeMap::deserialize(d).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyframe::ParamValue;

    #[test]
    fn a_copy_shares_until_it_changes() {
        let mut a: ParamMap = [("x".to_string(), Param::new(ParamValue::Float(1.0)))].into_iter().collect();
        let b = a.clone();
        assert!(a.shares(&b));
        a.insert("y".into(), Param::new(ParamValue::Float(2.0)));
        assert!(!a.shares(&b));
        assert_eq!(b.len(), 1);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn json_is_the_map() {
        let m: BTreeMap<String, Param> = [("x".to_string(), Param::new(ParamValue::Float(1.0)))].into_iter().collect();
        let p = ParamMap::from(m.clone());
        assert_eq!(serde_json::to_string(&p).ok(), serde_json::to_string(&m).ok());
        let back: Option<ParamMap> = serde_json::from_str(&serde_json::to_string(&m).unwrap_or_default()).ok();
        assert_eq!(back, Some(p));
    }
}
