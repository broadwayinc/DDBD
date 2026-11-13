//! A record -> every key that makes it findable (ARCHITECTURE.md, "Query views").

use crate::codec::body::Meta;
use crate::codec::keys::*;
use crate::codec::loose::loosen;
use crate::codec::text::{rev_chars, rev_name};
use std::collections::BTreeSet;

/// Index sort parts for one index entry (exact, reversed, loose, initials).
pub fn index_sorts(name: &str, val: &Val) -> Vec<Vec<u8>> {
    let mut out = vec![sort_idx(S_IDX, name, &typed(val))];
    let compound = name.contains('.');
    match val {
        Val::Str(s) => {
            let rn = rev_name(name);
            out.push(sort_idx(S_REV, &rn, &typed_str(&rev_chars(s))));
            let l = loosen(s);
            if !l.text.is_empty() {
                out.push(sort_idx(S_LOOSE, name, &typed_str(&l.text)));
                out.push(sort_idx(S_LOOSE_REV, &rn, &typed_str(&rev_chars(&l.text))));
            }
            if l.has_syllable && !l.initials.is_empty() {
                out.push(sort_idx(S_INI, name, &typed_str(&l.initials)));
                out.push(sort_idx(S_INI_REV, &rn, &typed_str(&rev_chars(&l.initials))));
            }
        }
        _ if compound => out.push(sort_idx(S_REV, &rev_name(name), &typed(val))),
        _ => {}
    }
    out
}

pub fn all_sorts(m: &Meta) -> Vec<Vec<u8>> {
    let mut sorts = vec![vec![S_REC], sort_u64(S_UPD, m.updated), sort_u64(S_REFD, m.referenced_count_indexed)];
    for (n, v) in &m.index {
        sorts.extend(index_sorts(n, v));
    }
    sorts
}

/// Scopes the record lives in, before tag copies.
pub fn scopes(m: &Meta, grantees: &[String]) -> Vec<Scope> {
    let mk = |view: u8, owner: &str| Scope {
        view,
        owner: owner.to_string(),
        coll: m.collection.clone(),
        lvl: m.level.clone(),
        sub: m.sub_owner().to_string(),
        tag: String::new(),
    };
    let mut s = vec![mk(V_COLL, ""), mk(V_USER, &m.user_id)];
    if let Some(r) = &m.reference {
        s.push(mk(V_REF, r));
    }
    if m.is_private() {
        s.push(mk(V_PRIV, &m.user_id));
        for g in grantees {
            if g != &m.user_id {
                s.push(mk(V_PRIV, g));
            }
        }
    }
    s
}

/// Every scope with its tag copies.
pub fn tagged_scopes(m: &Meta, grantees: &[String]) -> Vec<Scope> {
    let mut out = vec![];
    for s in scopes(m, grantees) {
        for t in &m.tags {
            let mut ts = s.clone();
            ts.tag = t.clone();
            out.push(ts);
        }
        out.push(s);
    }
    out
}

pub fn record_keys(id: &str, m: &Meta, grantees: &[String]) -> BTreeSet<Vec<u8>> {
    let sorts = all_sorts(m);
    let mut keys = BTreeSet::new();
    for s in tagged_scopes(m, grantees) {
        let sb = s.bytes();
        for so in &sorts {
            let mut k = Vec::with_capacity(sb.len() + so.len() + RECORD_ID_LEN);
            k.extend_from_slice(&sb);
            k.extend_from_slice(so);
            k.extend_from_slice(id.as_bytes());
            keys.insert(k);
        }
    }
    keys.insert(owner_key(&m.user_id, id));
    if let Some(r) = &m.reference {
        keys.insert(refby_key(r, &m.user_id, &m.collection, &m.level, m.sub_owner(), id));
    }
    keys
}

/// Keys only one grantee gets for a private record.
pub fn grantee_keys(id: &str, m: &Meta, grantee: &str) -> BTreeSet<Vec<u8>> {
    let mut keys = BTreeSet::new();
    if !m.is_private() {
        return keys;
    }
    let sorts = all_sorts(m);
    let base = Scope {
        view: V_PRIV,
        owner: grantee.to_string(),
        coll: m.collection.clone(),
        lvl: m.level.clone(),
        sub: m.sub_owner().to_string(),
        tag: String::new(),
    };
    let mut scopes = vec![base.clone()];
    for t in &m.tags {
        let mut s = base.clone();
        s.tag = t.clone();
        scopes.push(s);
    }
    for s in scopes {
        let sb = s.bytes();
        for so in &sorts {
            keys.insert(cat(&cat(&sb, so), id.as_bytes()));
        }
    }
    keys
}

/// The `f` (referenced count) keys of a record, for a given count.
pub fn refcount_keys(id: &str, m: &Meta, grantees: &[String], count: u64) -> Vec<Vec<u8>> {
    let so = sort_u64(S_REFD, count);
    tagged_scopes(m, grantees).iter().map(|s| cat(&cat(&s.bytes(), &so), id.as_bytes())).collect()
}

/// Rewrites every `X` (refby) key in the layout of refby_v2 (collection, level and sub before the record
/// id), from the records themselves. Run once at start on a folder made before it (`meta` refby_v2).
pub fn rebuild_refby_keys(db: &super::Db) -> crate::error::ApiResult<usize> {
    use crate::store::{Batch, CF_IX, CF_REC};
    let store = &db.store;
    let mut wrote = 0usize;
    let mut todo: Vec<(String, Meta)> = vec![];
    store.scan(CF_REC, b"", &[END], true, |k, v| {
        if let Some((m, _)) = crate::codec::body::decode(v) {
            if m.reference.is_some() {
                todo.push((String::from_utf8_lossy(k).into_owned(), m));
            }
        }
        true
    })?;
    for chunk in todo.chunks(5000) {
        let mut b = Batch::new(store);
        for (id, m) in chunk {
            let r = m.reference.as_deref().unwrap_or("");
            b.delete(CF_IX, &refby_key_v1(r, &m.user_id, id));
            b.put(CF_IX, &refby_key(r, &m.user_id, &m.collection, &m.level, m.sub_owner(), id), b"");
            wrote += 1;
        }
        b.commit()?;
    }
    Ok(wrote)
}
