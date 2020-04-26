use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::controls::Control;
use crate::ldap::Ldap;
use crate::parse_filter;
use crate::protocol::LdapOp;
use crate::result::{LdapError, LdapResult, Result};

use tokio::sync::mpsc;
use tokio::time;

use lber::common::TagClass;
use lber::parse::parse_tag;
use lber::structure::{StructureTag, PL};
use lber::structures::{Boolean, Enumerated, Integer, OctetString, Sequence, Tag};
use lber::universal::Types;
use lber::IResult;

/// Possible values for search scope.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scope {
    /// Base object; search only the object named in the base DN.
    Base = 0,
    /// Search the objects immediately below the base DN.
    OneLevel = 1,
    /// Search the object named in the base DN and the whole subtree below it.
    Subtree = 2,
}

/// Possible values for alias dereferencing during search.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DerefAliases {
    /// Never dereference.
    Never = 0,
    /// Dereference while retrieving objects according to search scope.
    Searching = 1,
    /// Dereference while finding the base object.
    Finding = 2,
    /// Always dereference.
    Always = 3,
}

#[derive(Debug)]
pub enum SearchItem {
    Entry(StructureTag),
    Referral(StructureTag),
    Done(LdapResult, Vec<Control>),
}

/// Wrapper for the internal structure of a result entry.
#[derive(Debug, Clone)]
pub struct ResultEntry(pub(crate) StructureTag);

impl ResultEntry {
    #[doc(hidden)]
    pub fn new(st: StructureTag) -> ResultEntry {
        ResultEntry(st)
    }

    pub fn is_ref(&self) -> bool {
        self.0.id == 19
    }

    pub fn is_intermediate(&self) -> bool {
        self.0.id == 25
    }
}

/// Additional parameters for the Search operation.
#[derive(Clone, Debug)]
pub struct SearchOptions {
    pub(crate) deref: DerefAliases,
    pub(crate) typesonly: bool,
    pub(crate) timelimit: i32,
    pub(crate) sizelimit: i32,
}

impl SearchOptions {
    /// Create an instance of the structure with default values.
    pub fn new() -> Self {
        SearchOptions {
            deref: DerefAliases::Never,
            typesonly: false,
            timelimit: 0,
            sizelimit: 0,
        }
    }

    /// Set the method for dereferencing aliases.
    pub fn deref(mut self, d: DerefAliases) -> Self {
        self.deref = d;
        self
    }

    /// Set the indicator of returning just attribute names (`true`) vs. names and values (`false`).
    pub fn typesonly(mut self, typesonly: bool) -> Self {
        self.typesonly = typesonly;
        self
    }

    /// Set the time limit, in seconds, for the whole search operation.
    ///
    /// This is a server-side limit of the elapsed time for performing the operation, _not_ a
    /// network timeout for retrieving result entries or the result of the whole operation.
    pub fn timelimit(mut self, timelimit: i32) -> Self {
        self.timelimit = timelimit;
        self
    }

    /// Set the size limit, in entries, for the whole search operation.
    pub fn sizelimit(mut self, sizelimit: i32) -> Self {
        self.sizelimit = sizelimit;
        self
    }
}

/// Parsed search result entry.
///
/// While LDAP attributes can have a variety of syntaxes, they're all returned in
/// search results as octet strings, without any associated type information. A
/// general-purpose result parser could leave all values in that format, but then
/// retrieving them from user code would be cumbersome and tedious.
///
/// For that reason, the parser tries to convert every value into a `String`. If an
/// attribute can contain unconstrained binary strings, the conversion may fail. In that case,
/// the attribute and all its values will be in the `bin_attrs` hashmap. Since it's
/// possible that a particular set of values for a binary attribute _could_ be
/// converted into UTF-8 `String`s, the presence of of such attribute in the result
/// entry should be checked for both in `attrs` and `bin_atrrs`.
///
/// In the future versions of the library, this parsing interface will be
/// de-emphasized in favor of custom Serde deserialization of search results directly
/// into a user-supplied struct, which is expected to be a better fit for the
/// majority of uses.
#[derive(Debug, Clone)]
pub struct SearchEntry {
    /// Entry DN.
    pub dn: String,
    /// Attributes.
    pub attrs: HashMap<String, Vec<String>>,
    /// Binary-valued attributes.
    pub bin_attrs: HashMap<String, Vec<Vec<u8>>>,
}

impl SearchEntry {
    /// Parse raw BER data and convert it into attribute map(s).
    ///
    /// __Note__: this function will panic on parsing error. Error handling will be
    /// improved in a future version of the library.
    pub fn construct(re: ResultEntry) -> SearchEntry {
        let mut tags =
            re.0.match_id(4)
                .and_then(|t| t.expect_constructed())
                .expect("entry")
                .into_iter();
        let dn = String::from_utf8(
            tags.next()
                .expect("element")
                .expect_primitive()
                .expect("octet string"),
        )
        .expect("dn");
        let mut attr_vals = HashMap::new();
        let mut bin_attr_vals = HashMap::new();
        let attrs = tags
            .next()
            .expect("element")
            .expect_constructed()
            .expect("attrs")
            .into_iter();
        for a_v in attrs {
            let mut part_attr = a_v
                .expect_constructed()
                .expect("partial attribute")
                .into_iter();
            let a_type = String::from_utf8(
                part_attr
                    .next()
                    .expect("element")
                    .expect_primitive()
                    .expect("octet string"),
            )
            .expect("attribute type");
            let mut any_binary = false;
            let values = part_attr
                .next()
                .expect("element")
                .expect_constructed()
                .expect("values")
                .into_iter()
                .map(|t| t.expect_primitive().expect("octet string"))
                .filter_map(|s| {
                    if let Ok(s) = std::str::from_utf8(s.as_ref()) {
                        return Some(s.to_owned());
                    }
                    bin_attr_vals
                        .entry(a_type.clone())
                        .or_insert_with(|| vec![])
                        .push(s);
                    any_binary = true;
                    None
                })
                .collect::<Vec<String>>();
            if any_binary {
                bin_attr_vals.get_mut(&a_type).expect("bin vector").extend(
                    values
                        .into_iter()
                        .map(String::into_bytes)
                        .collect::<Vec<Vec<u8>>>(),
                );
            } else {
                attr_vals.insert(a_type, values);
            }
        }
        SearchEntry {
            dn: dn,
            attrs: attr_vals,
            bin_attrs: bin_attr_vals,
        }
    }
}

#[derive(Debug)]
pub struct SearchStream {
    ldap: Ldap,
    rx: Option<mpsc::UnboundedReceiver<SearchItem>>,
    req: Option<Tag>,
    timeout: Option<Duration>,
    res: Option<LdapResult>,
}

impl SearchStream {
    pub(crate) fn new(ldap: Ldap) -> Self {
        SearchStream {
            ldap,
            rx: None,
            req: None,
            timeout: None,
            res: None,
        }
    }

    /// See also [`LdapConn::streaming_search()`](struct.LdapConn.html#method.streaming_search).
    ///
    /// The returned future resolves to a [`SearchStream`](struct.SearchStream.html),
    /// which should be iterated through to obtain results. Before starting the iteration,
    /// the receiver future, which will yield the overall result of the search after the stream
    /// is drained, should be retrieved from the stream instance with
    /// [`get_result_rx()`](struct.SearchStream.html#method.get_result_rx). The stream and
    /// the receiver should be polled concurrently with `Future::join()`.
    pub(crate) async fn start<S: AsRef<str>>(
        mut self,
        base: &str,
        scope: Scope,
        filter: &str,
        attrs: Vec<S>,
    ) -> Result<Self> {
        let opts = match self.ldap.search_opts.take() {
            Some(opts) => opts,
            None => SearchOptions::new(),
        };
        self.timeout = self.ldap.timeout.take();
        let req = Tag::Sequence(Sequence {
            id: 3,
            class: TagClass::Application,
            inner: vec![
                Tag::OctetString(OctetString {
                    inner: Vec::from(base.as_bytes()),
                    ..Default::default()
                }),
                Tag::Enumerated(Enumerated {
                    inner: scope as i64,
                    ..Default::default()
                }),
                Tag::Enumerated(Enumerated {
                    inner: opts.deref as i64,
                    ..Default::default()
                }),
                Tag::Integer(Integer {
                    inner: opts.sizelimit as i64,
                    ..Default::default()
                }),
                Tag::Integer(Integer {
                    inner: opts.timelimit as i64,
                    ..Default::default()
                }),
                Tag::Boolean(Boolean {
                    inner: opts.typesonly,
                    ..Default::default()
                }),
                match parse_filter(filter) {
                    Ok(filter) => filter,
                    _ => return Err(LdapError::FilterParsing),
                },
                Tag::Sequence(Sequence {
                    inner: attrs
                        .into_iter()
                        .map(|s| {
                            Tag::OctetString(OctetString {
                                inner: Vec::from(s.as_ref()),
                                ..Default::default()
                            })
                        })
                        .collect(),
                    ..Default::default()
                }),
            ],
        });
        let (tx, rx) = mpsc::unbounded_channel();
        self.rx = Some(rx);
        if let Some(ref timeout) = self.timeout {
            self.ldap.with_timeout(*timeout);
        }
        self.ldap.op_call(LdapOp::Search(tx), req).await?;
        Ok(self)
    }

    pub async fn next(&mut self) -> Result<Option<ResultEntry>> {
        if self.rx.is_none() {
            return Ok(None);
        }
        let item = if let Some(ref timeout) = self.timeout {
            time::timeout(*timeout, self.rx.as_mut().unwrap().recv()).await?
        } else {
            self.rx.as_mut().unwrap().recv().await
        };
        let item = match item {
            Some(item) => item,
            None => {
                self.rx = None;
                return Err(LdapError::EndOfStream);
            }
        };
        match item {
            SearchItem::Entry(tag) | SearchItem::Referral(tag) => {
                return Ok(Some(ResultEntry(tag)))
            }
            SearchItem::Done(mut res, controls) => {
                res.ctrls = controls;
                self.res = Some(res);
                self.rx = None;
            }
        }
        Ok(None)
    }

    pub fn finish(mut self) -> LdapResult {
        self.rx = None;
        self.res.unwrap_or_else(|| LdapResult {
            rc: 88,
            matched: String::from(""),
            text: String::from("user cancelled"),
            refs: vec![],
            ctrls: vec![],
        })
    }
}

#[derive(Clone, Debug)]
pub enum SyncInfo {
    NewCookie(Vec<u8>),
    RefreshDelete {
        cookie: Option<Vec<u8>>,
        refresh_done: bool,
    },
    RefreshPresent {
        cookie: Option<Vec<u8>>,
        refresh_done: bool,
    },
    SyncIdSet {
        cookie: Option<Vec<u8>>,
        refresh_deletes: bool,
        sync_uuids: HashSet<Vec<u8>>,
    },
}

pub fn parse_syncinfo<V: AsRef<[u8]>>(raw: V) -> SyncInfo {
    let syncinfo_val = match parse_tag(raw.as_ref()) {
        IResult::Done(_, tag) => tag,
        _ => panic!("syncinfo_val: not a tag"),
    };
    match syncinfo_val {
        StructureTag { id, class, payload } if class == TagClass::Context && id < 4 => match id {
            0 => {
                let cookie = match payload {
                    PL::P(payload) => payload,
                    PL::C(_) => panic!(),
                };
                SyncInfo::NewCookie(cookie)
            }
            1 | 2 | 3 => {
                let mut syncinfo_val = match payload {
                    PL::C(payload) => payload,
                    PL::P(_) => panic!(),
                }
                .into_iter();
                let mut sync_cookie = None;
                let mut flag = id != 3;
                let mut uuids = HashSet::new();
                let mut pass = 1;
                'it: loop {
                    match syncinfo_val.next() {
                        None => break 'it,
                        Some(comp) => match comp {
                            StructureTag {
                                id,
                                class,
                                payload: _,
                            } if class == TagClass::Universal
                                && id == Types::OctetString as u64
                                && pass <= 1 =>
                            {
                                sync_cookie = comp.expect_primitive();
                            }
                            StructureTag {
                                id,
                                class,
                                payload: _,
                            } if class == TagClass::Universal
                                && id == Types::Boolean as u64
                                && pass <= 2 =>
                            {
                                flag = !(comp.expect_primitive().expect("octet string")[0] == 0);
                            }
                            StructureTag {
                                id,
                                class,
                                payload: _,
                            } if class == TagClass::Universal
                                && id == Types::Set as u64
                                && pass <= 3 =>
                            {
                                uuids = comp
                                    .expect_constructed()
                                    .expect("uuid set")
                                    .into_iter()
                                    .map(|u| u.expect_primitive().expect("octet string"))
                                    .collect();
                            }
                            _ => panic!(),
                        },
                    }
                    pass += 1;
                }
                match id {
                    1 => SyncInfo::RefreshDelete {
                        cookie: sync_cookie,
                        refresh_done: flag,
                    },
                    2 => SyncInfo::RefreshPresent {
                        cookie: sync_cookie,
                        refresh_done: flag,
                    },
                    3 => SyncInfo::SyncIdSet {
                        cookie: sync_cookie,
                        refresh_deletes: flag,
                        sync_uuids: uuids,
                    },
                    _ => panic!(),
                }
            }
            _ => panic!(),
        },
        _ => panic!("syncinfo id not 0-3"),
    }
}

pub fn parse_refs(t: StructureTag) -> Vec<String> {
    t.expect_constructed()
        .expect("referrals")
        .into_iter()
        .map(|t| t.expect_primitive().expect("octet string"))
        .map(String::from_utf8)
        .map(|s| s.expect("uri"))
        .collect()
}
