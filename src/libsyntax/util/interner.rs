// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! An "interner" is a data structure that associates values with uint tags and
//! allows bidirectional lookup; i.e. given a value, one can easily find the
//! type, and vice versa.

use ast::Name;

use std::collections::HashMap;
use std::cell::RefCell;
use std::cmp::Equiv;
use std::fmt;
use std::hash::Hash;
use std::rc::Rc;

pub struct Interner<T> {
    map: RefCell<HashMap<T, Name>>,
    vect: RefCell<Vec<T> >,
}

// when traits can extend traits, we should extend index<Name,T> to get []
impl<T: Eq + Hash + Clone + 'static> Interner<T> {
    pub fn new() -> Interner<T> {
        Interner {
            map: RefCell::new(HashMap::new()),
            vect: RefCell::new(Vec::new()),
        }
    }

    pub fn prefill(init: &[T]) -> Interner<T> {
        let rv = Interner::new();
        for v in init.iter() {
            rv.intern((*v).clone());
        }
        rv
    }

    pub fn intern(&self, val: T) -> Name {
        let mut map = self.map.borrow_mut();
        match (*map).find(&val) {
            Some(&idx) => return idx,
            None => (),
        }

        let mut vect = self.vect.borrow_mut();
        let new_idx = Name((*vect).len() as u32);
        (*map).insert(val.clone(), new_idx);
        (*vect).push(val);
        new_idx
    }

    pub fn gensym(&self, val: T) -> Name {
        let mut vect = self.vect.borrow_mut();
        let new_idx = Name((*vect).len() as u32);
        // leave out of .map to avoid colliding
        (*vect).push(val);
        new_idx
    }

    pub fn get(&self, idx: Name) -> T {
        let vect = self.vect.borrow();
        (*vect)[idx.uint()].clone()
    }

    pub fn len(&self) -> uint {
        let vect = self.vect.borrow();
        (*vect).len()
    }

    pub fn find_equiv<Sized? Q: Hash + Equiv<T>>(&self, val: &Q) -> Option<Name> {
        let map = self.map.borrow();
        match (*map).find_equiv(val) {
            Some(v) => Some(*v),
            None => None,
        }
    }

    pub fn clear(&self) {
        *self.map.borrow_mut() = HashMap::new();
        *self.vect.borrow_mut() = Vec::new();
    }
}

#[deriving(Clone, PartialEq, Hash, PartialOrd)]
pub struct RcStr {
    string: Rc<String>,
}

impl Eq for RcStr {}

impl Ord for RcStr {
    fn cmp(&self, other: &RcStr) -> Ordering {
        self.as_slice().cmp(&other.as_slice())
    }
}

impl Str for RcStr {
    #[inline]
    fn as_slice<'a>(&'a self) -> &'a str {
        let s: &'a str = self.string.as_slice();
        s
    }
}

impl fmt::Show for RcStr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use std::fmt::Show;
        self.as_slice().fmt(f)
    }
}

impl RcStr {
    pub fn new(string: &str) -> RcStr {
        RcStr {
            string: Rc::new(string.into_string()),
        }
    }
}

/// A StrInterner differs from Interner<String> in that it accepts
/// &str rather than RcStr, resulting in less allocation.
pub struct StrInterner {
    map: RefCell<HashMap<RcStr, Name>>,
    vect: RefCell<Vec<RcStr> >,
}

/// When traits can extend traits, we should extend index<Name,T> to get []
impl StrInterner {
    pub fn new() -> StrInterner {
        StrInterner {
            map: RefCell::new(HashMap::new()),
            vect: RefCell::new(Vec::new()),
        }
    }

    pub fn prefill(init: &[&str]) -> StrInterner {
        let rv = StrInterner::new();
        for &v in init.iter() { rv.intern(v); }
        rv
    }

    pub fn intern(&self, val: &str) -> Name {
        let mut map = self.map.borrow_mut();
        match map.find_equiv(val) {
            Some(&idx) => return idx,
            None => (),
        }

        let new_idx = Name(self.len() as u32);
        let val = RcStr::new(val);
        map.insert(val.clone(), new_idx);
        self.vect.borrow_mut().push(val);
        new_idx
    }

    pub fn gensym(&self, val: &str) -> Name {
        let new_idx = Name(self.len() as u32);
        // leave out of .map to avoid colliding
        self.vect.borrow_mut().push(RcStr::new(val));
        new_idx
    }

    // I want these gensyms to share name pointers
    // with existing entries. This would be automatic,
    // except that the existing gensym creates its
    // own managed ptr using to_managed. I think that
    // adding this utility function is the most
    // lightweight way to get what I want, though not
    // necessarily the cleanest.

    /// Create a gensym with the same name as an existing
    /// entry.
    pub fn gensym_copy(&self, idx : Name) -> Name {
        let new_idx = Name(self.len() as u32);
        // leave out of map to avoid colliding
        let mut vect = self.vect.borrow_mut();
        let existing = (*vect)[idx.uint()].clone();
        vect.push(existing);
        new_idx
    }

    pub fn get(&self, idx: Name) -> RcStr {
        (*self.vect.borrow())[idx.uint()].clone()
    }

    pub fn len(&self) -> uint {
        self.vect.borrow().len()
    }

    pub fn find_equiv<Sized? Q:Hash + Equiv<RcStr>>(&self, val: &Q) -> Option<Name> {
        match (*self.map.borrow()).find_equiv(val) {
            Some(v) => Some(*v),
            None => None,
        }
    }

    pub fn clear(&self) {
        *self.map.borrow_mut() = HashMap::new();
        *self.vect.borrow_mut() = Vec::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ast::Name;

    #[test]
    #[should_fail]
    fn i1 () {
        let i : Interner<RcStr> = Interner::new();
        i.get(Name(13));
    }

    #[test]
    fn interner_tests () {
        let i : Interner<RcStr> = Interner::new();
        // first one is zero:
        assert_eq!(i.intern(RcStr::new("dog")), Name(0));
        // re-use gets the same entry:
        assert_eq!(i.intern(RcStr::new("dog")), Name(0));
        // different string gets a different #:
        assert_eq!(i.intern(RcStr::new("cat")), Name(1));
        assert_eq!(i.intern(RcStr::new("cat")), Name(1));
        // dog is still at zero
        assert_eq!(i.intern(RcStr::new("dog")), Name(0));
        // gensym gets 3
        assert_eq!(i.gensym(RcStr::new("zebra") ), Name(2));
        // gensym of same string gets new number :
        assert_eq!(i.gensym (RcStr::new("zebra") ), Name(3));
        // gensym of *existing* string gets new number:
        assert_eq!(i.gensym(RcStr::new("dog")), Name(4));
        assert_eq!(i.get(Name(0)), RcStr::new("dog"));
        assert_eq!(i.get(Name(1)), RcStr::new("cat"));
        assert_eq!(i.get(Name(2)), RcStr::new("zebra"));
        assert_eq!(i.get(Name(3)), RcStr::new("zebra"));
        assert_eq!(i.get(Name(4)), RcStr::new("dog"));
    }

    #[test]
    fn i3 () {
        let i : Interner<RcStr> = Interner::prefill([
            RcStr::new("Alan"),
            RcStr::new("Bob"),
            RcStr::new("Carol")
        ]);
        assert_eq!(i.get(Name(0)), RcStr::new("Alan"));
        assert_eq!(i.get(Name(1)), RcStr::new("Bob"));
        assert_eq!(i.get(Name(2)), RcStr::new("Carol"));
        assert_eq!(i.intern(RcStr::new("Bob")), Name(1));
    }

    #[test]
    fn string_interner_tests() {
        let i : StrInterner = StrInterner::new();
        // first one is zero:
        assert_eq!(i.intern("dog"), Name(0));
        // re-use gets the same entry:
        assert_eq!(i.intern ("dog"), Name(0));
        // different string gets a different #:
        assert_eq!(i.intern("cat"), Name(1));
        assert_eq!(i.intern("cat"), Name(1));
        // dog is still at zero
        assert_eq!(i.intern("dog"), Name(0));
        // gensym gets 3
        assert_eq!(i.gensym("zebra"), Name(2));
        // gensym of same string gets new number :
        assert_eq!(i.gensym("zebra"), Name(3));
        // gensym of *existing* string gets new number:
        assert_eq!(i.gensym("dog"), Name(4));
        // gensym tests again with gensym_copy:
        assert_eq!(i.gensym_copy(Name(2)), Name(5));
        assert_eq!(i.get(Name(5)), RcStr::new("zebra"));
        assert_eq!(i.gensym_copy(Name(2)), Name(6));
        assert_eq!(i.get(Name(6)), RcStr::new("zebra"));
        assert_eq!(i.get(Name(0)), RcStr::new("dog"));
        assert_eq!(i.get(Name(1)), RcStr::new("cat"));
        assert_eq!(i.get(Name(2)), RcStr::new("zebra"));
        assert_eq!(i.get(Name(3)), RcStr::new("zebra"));
        assert_eq!(i.get(Name(4)), RcStr::new("dog"));
    }
}
