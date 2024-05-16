use std::sync::Arc;

use semver_pubgrub::SemverCompatibility;

#[derive(Clone, Eq, PartialEq, Hash)]
pub enum Names {
    Bucket(Arc<str>, SemverCompatibility, bool),
    BucketFeatures(Arc<str>, SemverCompatibility, Arc<str>),
    Wide(
        Arc<str>,
        Box<semver::VersionReq>,
        Arc<str>,
        SemverCompatibility,
    ),
    WideFeatures(
        Arc<str>,
        Box<semver::VersionReq>,
        Arc<str>,
        SemverCompatibility,
        Arc<str>,
    ),
    Links(Arc<str>),
}

pub fn new_bucket(
    crate_: impl Into<Arc<str>>,
    compat: SemverCompatibility,
    all_features: bool,
) -> Box<Names> {
    Box::new(Names::Bucket(crate_.into(), compat, all_features))
}
pub fn new_wide(
    crate_: impl Into<Arc<str>>,
    req: impl Into<Box<semver::VersionReq>>,
    from: impl Into<Arc<str>>,
    compat: SemverCompatibility,
) -> Box<Names> {
    Box::new(Names::Wide(crate_.into(), req.into(), from.into(), compat))
}
pub fn new_links(crate_: impl Into<Arc<str>>) -> Box<Names> {
    Box::new(Names::Links(crate_.into()))
}

impl Ord for Names {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Names::Bucket(l1, l2, l3), Names::Bucket(r1, r2, r3)) => {
                (l1, l2, l3).cmp(&(r1, r2, r3))
            }
            (Names::Bucket(_, _, _), _) => Ordering::Greater,
            (Names::BucketFeatures(_, _, _), Names::Bucket(_, _, _)) => Ordering::Less,
            (Names::BucketFeatures(l1, l2, l3), Names::BucketFeatures(r1, r2, r3)) => {
                (l1, l2, l3).cmp(&(r1, r2, r3))
            }
            (Names::BucketFeatures(_, _, _), _) => Ordering::Greater,
            (Names::Wide(_, _, _, _), Names::Bucket(_, _, _)) => Ordering::Less,
            (Names::Wide(_, _, _, _), Names::BucketFeatures(_, _, _)) => Ordering::Less,
            (Names::Wide(l1, lreq, l2, l3), Names::Wide(r1, rreq, r2, r3)) => {
                (l1, l2, l3).cmp(&(r1, r2, r3)).then_with(|| {
                    if lreq == rreq {
                        Ordering::Equal
                    } else {
                        lreq.to_string().cmp(&rreq.to_string())
                    }
                })
            }
            (Names::Wide(_, _, _, _), _) => Ordering::Greater,
            (Names::WideFeatures(_, _, _, _, _), Names::Bucket(_, _, _)) => Ordering::Less,
            (Names::WideFeatures(_, _, _, _, _), Names::BucketFeatures(_, _, _)) => Ordering::Less,
            (Names::WideFeatures(_, _, _, _, _), Names::Wide(_, _, _, _)) => Ordering::Less,
            (
                Names::WideFeatures(l1, lreq, l2, l3, l4),
                Names::WideFeatures(r1, rreq, r2, r3, r4),
            ) => (l1, l2, l3, l4).cmp(&(r1, r2, r3, r4)).then_with(|| {
                if lreq == rreq {
                    Ordering::Equal
                } else {
                    lreq.to_string().cmp(&rreq.to_string())
                }
            }),
            (Names::WideFeatures(_, _, _, _, _), _) => Ordering::Greater,
            (Names::Links(l), Names::Links(r)) => l.cmp(r),
            (Names::Links(_), _) => Ordering::Less,
        }
    }
}

impl PartialOrd for Names {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Names {
    pub fn is_real(&self) -> bool {
        matches!(self, &Self::Bucket(..))
    }
    pub fn crate_(&self) -> Arc<str> {
        match self {
            Names::Bucket(c, _, _) => c.clone(),
            Names::BucketFeatures(c, _, _) => c.clone(),
            Names::Wide(c, _, _, _) => c.clone(),
            Names::WideFeatures(c, _, _, _, _) => c.clone(),
            Names::Links(_) => panic!(),
        }
    }
    pub fn with_features(&self, feat: impl Into<Arc<str>>) -> Box<Self> {
        let feat = feat.into();
        Box::new(match self {
            Names::Bucket(a, b, _) => Names::BucketFeatures(a.clone(), b.clone(), feat),
            Names::BucketFeatures(a, b, _) => Names::BucketFeatures(a.clone(), b.clone(), feat),
            Names::Wide(a, b, c, d) => {
                Names::WideFeatures(a.clone(), b.clone(), c.clone(), d.clone(), feat)
            }
            Names::WideFeatures(a, b, c, d, _) => {
                Names::WideFeatures(a.clone(), b.clone(), c.clone(), d.clone(), feat)
            }
            Names::Links(_) => panic!(),
        })
    }
}

impl std::fmt::Display for Names {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Names::Bucket(n, m, a) => {
                f.write_str("Bucket:")?;
                f.write_str(n)?;
                f.write_str("@")?;
                f.write_str(&match m {
                    SemverCompatibility::Major(i) => format!("{}.x.y", i),
                    SemverCompatibility::Minor(i) => format!("0.{}.x", i),
                    SemverCompatibility::Patch(i) => format!("0.0.{}", i),
                })?;
                if *a {
                    f.write_str("/All-FEATURES")?;
                }
                Ok(())
            }
            Names::BucketFeatures(n, m, o) => {
                f.write_str("Bucket:")?;
                f.write_str(n)?;
                f.write_str("@")?;
                f.write_str(&match m {
                    SemverCompatibility::Major(i) => format!("{}.x.y", i),
                    SemverCompatibility::Minor(i) => format!("0.{}.x", i),
                    SemverCompatibility::Patch(i) => format!("0.0.{}", i),
                })?;
                f.write_str("/")?;
                f.write_str(o)
            }
            Names::Wide(c, range, parent, parent_com) => {
                f.write_str("Range:")?;
                f.write_str(c)?;
                f.write_str("(From:")?;
                f.write_str(parent)?;
                f.write_str("@")?;
                f.write_str(&match parent_com {
                    SemverCompatibility::Major(i) => format!("{}.x.y", i),
                    SemverCompatibility::Minor(i) => format!("0.{}.x", i),
                    SemverCompatibility::Patch(i) => format!("0.0.{}", i),
                })?;
                f.write_str("):")?;
                f.write_str(&range.to_string())
            }
            Names::WideFeatures(c, range, parent, parent_com, feat) => {
                f.write_str("Range:")?;
                f.write_str(c)?;
                f.write_str("(From:")?;
                f.write_str(parent)?;
                f.write_str("@")?;
                f.write_str(&match parent_com {
                    SemverCompatibility::Major(i) => format!("{}.x.y", i),
                    SemverCompatibility::Minor(i) => format!("0.{}.x", i),
                    SemverCompatibility::Patch(i) => format!("0.0.{}", i),
                })?;
                f.write_str("):")?;
                f.write_str(&range.to_string())?;
                f.write_str("/")?;
                f.write_str(feat)
            }
            Names::Links(name) => {
                f.write_str("Links:")?;
                f.write_str(name)
            }
        }
    }
}

impl std::fmt::Debug for Names {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self, f)
    }
}

impl serde::Serialize for Names {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
