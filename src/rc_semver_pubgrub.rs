use std::{cell::RefCell, collections::HashMap, rc::Rc};

use pubgrub::VersionSet;
use semver_pubgrub::{SemverPubgrub, SmallVersion};

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum RcSemverPubgrub {
    Owned(Rc<SemverPubgrub<SmallVersion>>),
    Borrowed(&'static SemverPubgrub<SmallVersion>),
}

impl serde::Serialize for RcSemverPubgrub {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.inner().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for RcSemverPubgrub {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let inner = SemverPubgrub::deserialize(deserializer)?;
        Ok(RcSemverPubgrub::new_owned(inner))
    }
}

impl RcSemverPubgrub {
    pub fn new_owned(inner: SemverPubgrub<SmallVersion>) -> Self {
        Self::Owned(Rc::new(inner))
    }

    pub fn new_borrowed(inner: &'static SemverPubgrub<SmallVersion>) -> Self {
        Self::Borrowed(inner)
    }

    pub fn inner(&self) -> &SemverPubgrub<SmallVersion> {
        match self {
            RcSemverPubgrub::Owned(v) => v,
            RcSemverPubgrub::Borrowed(v) => v,
        }
    }
}

impl std::fmt::Display for RcSemverPubgrub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner().fmt(f)
    }
}

thread_local! {
    static ARC_SEMVER_PUBGRUB_EMPTY: RcSemverPubgrub = RcSemverPubgrub::new_owned(SemverPubgrub::empty());

    static ARC_SEMVER_PUBGRUB_SINGLETON: RefCell<HashMap<SmallVersion, RcSemverPubgrub>> = RefCell::new(HashMap::default());
}

impl VersionSet for RcSemverPubgrub {
    type V = <SemverPubgrub<SmallVersion> as VersionSet>::V;

    fn empty() -> Self {
        ARC_SEMVER_PUBGRUB_EMPTY.with(|v| v.clone())
    }

    fn singleton(v: Self::V) -> Self {
        ARC_SEMVER_PUBGRUB_SINGLETON.with_borrow_mut(|map| {
            map.entry(v)
                .or_insert_with_key(|v| {
                    RcSemverPubgrub::new_owned(SemverPubgrub::singleton(v.clone()))
                })
                .clone()
        })
    }

    fn complement(&self) -> Self {
        RcSemverPubgrub::new_owned(self.inner().complement())
    }

    fn intersection(&self, other: &Self) -> Self {
        if std::ptr::eq(self.inner(), other.inner()) {
            return self.clone();
        }
        RcSemverPubgrub::new_owned(self.inner().intersection(&other.inner()))
    }

    fn contains(&self, v: &Self::V) -> bool {
        self.inner().contains(v)
    }

    fn full() -> Self {
        RcSemverPubgrub::new_owned(SemverPubgrub::full())
    }

    fn union(&self, other: &Self) -> Self {
        if std::ptr::eq(self.inner(), other.inner()) {
            return self.clone();
        }
        RcSemverPubgrub::new_owned(self.inner().union(&other.inner()))
    }

    fn is_disjoint(&self, other: &Self) -> bool {
        if std::ptr::eq(self.inner(), other.inner()) {
            return false;
        }
        self.inner().is_disjoint(&other.inner())
    }

    fn subset_of(&self, other: &Self) -> bool {
        if std::ptr::eq(self.inner(), other.inner()) {
            return true;
        }
        self.inner().subset_of(&other.inner())
    }
}
