use basics::data::SizeOf;
use basics::{DataType, Record};
use std::borrow::Cow;

use rand::{Rng, ThreadRng};
use std::sync::Arc;
use std::sync::Mutex;
use std::collections::HashMap;
use fnv::FnvBuildHasher;

/// Allocate a new end-user facing result table.
pub(crate) fn new(srmap: bool, cols: usize, key: &[usize], uid: usize) -> (SingleReadHandle, WriteHandle) {
    new_inner(srmap, cols, key, None, uid)
}

/// Allocate a new partially materialized end-user facing result table.
///
/// Misses in this table will call `trigger` to populate the entry, and retry until successful.
pub(crate) fn new_partial<F>(
    srmap: bool,
    cols: usize,
    key: &[usize],
    trigger: F,
    uid: usize,
) -> (SingleReadHandle, WriteHandle)
where
    F: Fn(&[DataType]) + 'static + Send + Sync,
{
    new_inner(srmap, cols, key, Some(Arc::new(trigger)), uid)
}

fn new_inner(
    srmap: bool,
    cols: usize,
    key: &[usize],
    trigger: Option<Arc<Fn(&[DataType]) + Send + Sync>>,
    uid: usize,
) -> (SingleReadHandle, WriteHandle) {
    let contiguous = {
        let mut contiguous = true;
        let mut last = None;
        for &k in key {
            if let Some(last) = last {
                if k != last + 1 {
                    contiguous = false;
                    break;
                }
            }
            last = Some(k);
        }
        contiguous
    };

    let mut srmap = false;

    macro_rules! make_srmap {
    ($variant:tt) => {{
            use srmap;
            // println!("actually making srmap nice");
            let (r, w) = srmap::srmap::construct(-1);
            (multir::Handle::$variant(r), multiw::Handle::$variant(w))
        }};
    }

    macro_rules! make {
        ($variant:tt) => {{
            use evmap;
            let (r, w) = evmap::Options::default()
                .with_meta(-1)
                .with_hasher(FnvBuildHasher::default())
                .construct();
            (multir::Handle::$variant(r), multiw::Handle::$variant(w))
        }};
    }

    srmap = true;
    let (r, w) = match (key.len(), srmap) {
        (0, _) => unreachable!(),
        (1, true) => make_srmap!(SingleSR),
        // (1, false) => make!(Single),
        (2, true) => make_srmap!(DoubleSR),
        // (2, false) => make!(Double),
        (_, true) => make_srmap!(ManySR),
        (_, false) => unreachable!()
    };

    let w = WriteHandle {
        partial: trigger.is_some(),
        handle: w,
        key: Vec::from(key),
        cols: cols,
        contiguous,
        mem_size: 0,
        uid: uid.clone()
    };
    let r = SingleReadHandle {
        handle: r,
        trigger: trigger,
        key: Vec::from(key),
        uid: uid.clone()
    };

    (r, w)
}

mod multir;
mod multiw;

fn key_to_single<'a>(k: Key<'a>) -> Cow<'a, DataType> {
    assert_eq!(k.len(), 1);
    match k {
        Cow::Owned(mut k) => Cow::Owned(k.swap_remove(0)),
        Cow::Borrowed(k) => Cow::Borrowed(&k[0]),
    }
}

fn key_to_double<'a>(k: Key<'a>) -> Cow<'a, (DataType, DataType)> {
    assert_eq!(k.len(), 2);
    match k {
        Cow::Owned(k) => {
            let mut k = k.into_iter();
            let k1 = k.next().unwrap();
            let k2 = k.next().unwrap();
            Cow::Owned((k1, k2))
        }
        Cow::Borrowed(k) => Cow::Owned((k[0].clone(), k[1].clone())),
    }
}

#[derive(Clone)]
pub(crate) struct WriteHandle {
    handle: multiw::Handle,
    partial: bool,
    cols: usize,
    key: Vec<usize>,
    contiguous: bool,
    mem_size: usize,
    uid: usize,
}

type Key<'a> = Cow<'a, [DataType]>;
pub(crate) struct MutWriteHandleEntry<'a> {
    handle: &'a mut WriteHandle,
    key: Key<'a>,
}
pub(crate) struct WriteHandleEntry<'a> {
    handle: &'a WriteHandle,
    key: Key<'a>,
}

impl<'a> MutWriteHandleEntry<'a> {
    pub fn mark_filled(self) {
        if let Some((None, _)) = self
            .handle
            .handle
            .meta_get_and(Cow::Borrowed(&*self.key), |rs| rs.is_empty(), self.handle.uid.clone())
        {
            self.handle.handle.clear(self.key, self.handle.uid.clone())
        } else {
            unreachable!("attempted to fill already-filled key");
        }
    }

    pub fn mark_hole(self) {
        let size = self
            .handle
            .handle
            .meta_get_and(Cow::Borrowed(&*self.key), |rs| {
                rs.iter().map(|r| r.deep_size_of()).sum()
            }, self.handle.uid.clone()).map(|r| r.0.unwrap_or(0))
            .unwrap_or(0);
        self.handle.mem_size = self.handle.mem_size.checked_sub(size as usize).unwrap();
        self.handle.handle.empty(self.key, self.handle.uid.clone())
    }
}

impl<'a> WriteHandleEntry<'a> {
    pub(crate) fn try_find_and<F, T>(self, mut then: F) -> Result<(Option<T>, i64), ()>
    where
        F: FnMut(&[Vec<DataType>]) -> T,
    {
        self.handle
            .handle
            .meta_get_and(self.key, &mut then, self.handle.uid.clone())
            .ok_or(())
    }
}

fn key_from_record<'a, R>(key: &[usize], contiguous: bool, record: R) -> Key<'a>
where
    R: Into<Cow<'a, [DataType]>>,
{
    match record.into() {
        Cow::Owned(mut record) => {
            let mut i = 0;
            let mut keep = key.into_iter().peekable();
            record.retain(|_| {
                i += 1;
                if let Some(&&next) = keep.peek() {
                    if next != i - 1 {
                        return false;
                    }
                } else {
                    return false;
                }

                assert_eq!(*keep.next().unwrap(), i - 1);
                true
            });
            Cow::Owned(record)
        }
        Cow::Borrowed(record) if contiguous => Cow::Borrowed(&record[key[0]..(key[0] + key.len())]),
        Cow::Borrowed(record) => Cow::Owned(key.iter().map(|&i| &record[i]).cloned().collect()),
    }
}

impl WriteHandle {

    pub fn clone_with_uid (&self, uid: usize) -> WriteHandle {
        WriteHandle {
            handle: self.handle.clone(),
            partial: self.partial.clone(),
            cols: self.cols.clone(),
            key: self.key.clone(),
            contiguous: self.contiguous.clone(),
            mem_size: self.mem_size.clone(),
            uid: uid,
        }
    }

    pub(crate) fn universe(&self) -> usize{
        self.uid.clone()
    }

    pub(crate) fn mut_with_key<'a, K>(&'a mut self, key: K) -> MutWriteHandleEntry<'a>
    where
        K: Into<Key<'a>>,
    {
        MutWriteHandleEntry {
            handle: self,
            key: key.into(),
        }
    }

    pub(crate) fn add_user(&mut self, uid: usize) {
        self.handle.add_user(uid)
    }

    pub(crate) fn with_key<'a, K>(&'a self, key: K) -> WriteHandleEntry<'a>
    where
        K: Into<Key<'a>>,
    {
        WriteHandleEntry {
            handle: self,
            key: key.into(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn mut_entry_from_record<'a, R>(&'a mut self, record: R) -> MutWriteHandleEntry<'a>
    where
        R: Into<Cow<'a, [DataType]>>,
    {
        let key = key_from_record(&self.key[..], self.contiguous, record);
        self.mut_with_key(key)
    }

    pub(crate) fn entry_from_record<'a, R>(&'a self, record: R) -> WriteHandleEntry<'a>
    where
        R: Into<Cow<'a, [DataType]>>,
    {
        let key = key_from_record(&self.key[..], self.contiguous, record);
        self.with_key(key)
    }

    pub(crate) fn swap(&mut self) {
        self.handle.refresh();
    }

    /// Add a new set of records to the backlog.
    ///
    /// These will be made visible to readers after the next call to `swap()`.
    pub(crate) fn add<I>(&mut self, rs: I)
    where
        I: IntoIterator<Item = Record>,
    {
        let mem_delta = self.handle.add(&self.key[..], self.cols, rs, self.uid.clone());
        if mem_delta > 0 {
            self.mem_size += mem_delta as usize;
        } else if mem_delta < 0 {
            self.mem_size = self
                .mem_size
                .checked_sub(mem_delta.checked_abs().unwrap() as usize)
                .unwrap();
        }
    }

    pub(crate) fn is_partial(&self) -> bool {
        self.partial
    }

    /// Evict `count` randomly selected keys from state and return them along with the number of
    /// bytes that will be freed once the underlying `evmap` applies the operation.
    pub fn evict_random_key(&mut self, rng: &mut ThreadRng) -> u64 {
        let mut bytes_to_be_freed = 0;
        if self.mem_size > 0 {
            if self.handle.is_empty() {
                unreachable!("mem size is {}, but map is empty", self.mem_size);
            }

            match self.handle.empty_at_index(rng.gen()) {
                None => (),
                Some(vs) => {
                    let size: u64 = vs.into_iter().map(|r| r.deep_size_of() as u64).sum();
                    bytes_to_be_freed += size;
                }
            }
            self.mem_size = self
                .mem_size
                .checked_sub(bytes_to_be_freed as usize)
                .unwrap();
        }
        bytes_to_be_freed
    }
}

impl SizeOf for WriteHandle {
    fn size_of(&self) -> u64 {
        use std::mem::size_of;

        size_of::<Self>() as u64
    }

    fn deep_size_of(&self) -> u64 {
        self.mem_size as u64
    }
}

/// Handle to get the state of a single shard of a reader.
#[derive(Clone)]
pub struct SingleReadHandle {
    handle: multir::Handle,
    trigger: Option<Arc<Fn(&[DataType]) + Send + Sync>>,
    key: Vec<usize>,
    uid: usize
}

impl SingleReadHandle {
    pub fn clone_with_uid (&self, uid: usize) -> SingleReadHandle {
        SingleReadHandle {
            handle: self.handle.clone(),
            trigger: self.trigger.clone(),
            key: self.key.clone(),
            uid: uid
        }
    }

    pub fn universe(&self) -> usize{
        self.uid.clone()
    }

    /// Trigger a replay of a missing key from a partially materialized view.
    pub fn trigger(&self, key: &[DataType]) {
        assert!(
            self.trigger.is_some(),
            "tried to trigger a replay for a fully materialized view"
        );

        // trigger a replay to populate
        (*self.trigger.as_ref().unwrap())(key);
    }

    /// Find all entries that matched the given conditions.
    ///
    /// Returned records are passed to `then` before being returned.
    ///
    /// Note that not all writes will be included with this read -- only those that have been
    /// swapped in by the writer.
    ///
    /// Holes in partially materialized state are returned as `Ok((None, _))`.
    pub fn try_find_and<F, T>(&self, key: &[DataType], mut then: F) -> Result<(Option<T>, i64), ()>
    where
        F: FnMut(&[Vec<DataType>]) -> T,
    {
        self.handle
            .meta_get_and(key, &mut then, self.uid.clone())
            .ok_or(())
            .map(|(mut records, meta)| {
                if records.is_none() && self.trigger.is_none() {
                    records = Some(then(&[]));
                }
                (records, meta)
            })
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.handle.len()
    }

    /// Count the number of rows in the reader.
    /// This is a potentially very costly operation, since it will
    /// hold up writers until all rows are iterated through.
    pub fn count_rows(&self, uid: usize) -> usize {
        let mut nrows = 0;
        self.handle.for_each(|v| nrows += v.len(), uid);
        nrows
    }
}

#[derive(Clone)]
pub enum ReadHandle {
    Sharded(Vec<Option<SingleReadHandle>>),
    Singleton(Option<SingleReadHandle>),
}

impl ReadHandle {
    /// Find all entries that matched the given conditions.
    ///
    /// Returned records are passed to `then` before being returned.
    ///
    /// Note that not all writes will be included with this read -- only those that have been
    /// swapped in by the writer.
    ///
    /// A hole in partially materialized state is returned as `Ok((None, _))`.
    pub fn try_find_and<F, T>(&self, key: &[DataType], then: F) -> Result<(Option<T>, i64), ()>
    where
        F: FnMut(&[Vec<DataType>]) -> T,
    {
        match *self {
            ReadHandle::Sharded(ref shards) => {
                assert_eq!(key.len(), 1);
                shards[::shard_by(&key[0], shards.len())]
                    .as_ref()
                    .unwrap()
                    .try_find_and(key, then)
            }
            ReadHandle::Singleton(ref srh) => { let res = srh.as_ref().unwrap().try_find_and(key, then); res}
        }
    }

    pub fn len(&self) -> usize {
        match *self {
            ReadHandle::Sharded(ref shards) => {
                shards.iter().map(|s| s.as_ref().unwrap().len()).sum()
            }
            ReadHandle::Singleton(ref srh) => srh.as_ref().unwrap().len(),
        }
    }

    pub fn set_single_handle(&mut self, shard: Option<usize>, handle: SingleReadHandle) {
        match (self, shard) {
            (&mut ReadHandle::Singleton(ref mut srh), None) => {
                *srh = Some(handle);
            }
            (&mut ReadHandle::Sharded(ref mut rhs), None) => {
                // when ::SHARDS == 1, sharded domains think they're unsharded
                assert_eq!(rhs.len(), 1);
                let srh = rhs.get_mut(0).unwrap();
                assert!(srh.is_none());
                *srh = Some(handle)
            }
            (&mut ReadHandle::Sharded(ref mut rhs), Some(shard)) => {
                let srh = rhs.get_mut(shard).unwrap();
                assert!(srh.is_none());
                *srh = Some(handle)
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srmap_works() {
        let global: usize = 0 as usize;
        let u1: usize = 1 as usize;
        let u2: usize = 2 as usize;

        let rec1 = vec![1.into(), "a".into()];
        let rec2 = vec![2.into(), "b".into()];
        let rec1_ = vec![1.into(), "b".into()];

        // create new map
        let (r, mut w) = new(true, 2, &[0], global.clone());


        let r1 = r.clone_with_uid(u1.clone());
        let r2 = r.clone_with_uid(u2.clone());

        let mut w1 = w.clone_with_uid(u1.clone());
        let mut w2 = w.clone_with_uid(u2.clone());

        // Register SRMap users
        w.add_user(global.clone());
        w.add_user(u1.clone());
        w.add_user(u2.clone());

        w.add(vec![Record::Positive(rec1.clone())]);
        w1.add(vec![Record::Positive(rec2.clone())]);
        w2.add(vec![Record::Positive(rec1_.clone())]);

        //
        // w.insert(k.clone(), v.clone(), uid2.clone());
        // // println!("After second insert: {:?}", lock.read().unwrap());
        //
        // w.insert(k.clone(), v2.clone(), uid2.clone());
        // // println!("After overlapping insert: {:?}", lock.read().unwrap());
        //
        // let v = r.get_and(&k.clone(), |rs| { rs.iter().any(|r| *r == "x".to_string()) }, uid1.clone()).unwrap();
        // assert_eq!(v, true);
        //
        // let v = r.get_and(&k.clone(), |rs| { rs.iter().any(|r| *r == "x2".to_string()) }, uid2.clone()).unwrap();
        // assert_eq!(v, true);
        //
        // w.remove(k.clone(), uid1.clone());
        // // println!("After remove: {:?}", lock.read().unwrap());
        //
        // let v = r.get_and(&k.clone(), |rs| { false }, uid1.clone());
        // // println!("V: {:?}", v);
        // match v {
        //     Some(val) => assert_eq!(val, false),
        //     None => {}
        // };
        //
        // w.remove(k.clone(), uid2.clone());
        // // println!("After user specific remove {:?}", lock.read().unwrap());
        //
        // w.remove_user(uid1);
        // // println!("After removing u1 {:?}", lock.read().unwrap());
        //
        // w.remove_user(uid2);
        // // println!("After removing u2 {:?}", lock.read().unwrap());
    }

    // #[test]
    // fn store_works() {
    //     let a = vec![1.into(), "a".into()];
    //
    //     let (r, mut w) = new(2, &[0]);
    //
    //     // initially, store is uninitialized
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()), Err(()));
    //
    //     w.swap();
    //
    //     // after first swap, it is empty, but ready
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()), Ok((Some(0), -1)));
    //
    //     w.add(vec![Record::Positive(a.clone())]);
    //
    //     // it is empty even after an add (we haven't swapped yet)
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()), Ok((Some(0), -1)));
    //
    //     w.swap();
    //
    //     // but after the swap, the record is there!
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == a[0] && r[1] == a[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    // }
    //
    // #[test]
    // fn busybusybusy() {
    //     use std::thread;
    //
    //     let n = 10000;
    //     let (r, mut w) = new(1, &[0]);
    //     thread::spawn(move || {
    //         for i in 0..n {
    //             w.add(vec![Record::Positive(vec![i.into()])]);
    //             w.swap();
    //         }
    //     });
    //
    //     for i in 0..n {
    //         let i = &[i.into()];
    //         loop {
    //             match r.try_find_and(i, |rs| rs.len()) {
    //                 Ok((None, _)) => continue,
    //                 Ok((Some(1), _)) => break,
    //                 Ok((Some(i), _)) => assert_ne!(i, 1),
    //                 Err(()) => continue,
    //             }
    //         }
    //     }
    // }
    //
    // #[test]
    // fn minimal_query() {
    //     let a = vec![1.into(), "a".into()];
    //     let b = vec![1.into(), "b".into()];
    //
    //     let (r, mut w) = new(2, &[0]);
    //     w.add(vec![Record::Positive(a.clone())]);
    //     w.swap();
    //     w.add(vec![Record::Positive(b.clone())]);
    //
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == a[0] && r[1] == a[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    // }
    //
    // #[test]
    // fn non_minimal_query() {
    //     let a = vec![1.into(), "a".into()];
    //     let b = vec![1.into(), "b".into()];
    //     let c = vec![1.into(), "c".into()];
    //
    //     let (r, mut w) = new(2, &[0]);
    //     w.add(vec![Record::Positive(a.clone())]);
    //     w.add(vec![Record::Positive(b.clone())]);
    //     w.swap();
    //     w.add(vec![Record::Positive(c.clone())]);
    //
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(2));
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == a[0] && r[1] == a[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    // }
    //
    // #[test]
    // fn absorb_negative_immediate() {
    //     let a = vec![1.into(), "a".into()];
    //     let b = vec![1.into(), "b".into()];
    //
    //     let (r, mut w) = new(2, &[0]);
    //     w.add(vec![Record::Positive(a.clone())]);
    //     w.add(vec![Record::Positive(b.clone())]);
    //     w.add(vec![Record::Negative(a.clone())]);
    //     w.swap();
    //
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    // }
    //
    // // #[test]
    // // fn srmap_works() {
    // //     let k = "x".to_string();
    // //     let v = "x".to_string();
    // //     let v2 = "x2".to_string();
    // //     let uid1: usize = 0 as usize;
    // //     let uid2: usize = 1 as usize;
    // //
    // //     let (r1, mut w1) = new(true, cols: 2, key: &[0], uid1.clone());
    // //
    // //
    // //     // create two users
    // //     w.add_user(uid1);
    // //     w.add_user(uid2);
    // //
    // //     w.insert(k.clone(), v.clone(), uid1.clone());
    // //     let lock = r.get_lock();
    // //     // println!("After first insert: {:?}", lock.read().unwrap());
    // //
    // //     w.insert(k.clone(), v.clone(), uid2.clone());
    // //     // println!("After second insert: {:?}", lock.read().unwrap());
    // //
    // //     w.insert(k.clone(), v2.clone(), uid2.clone());
    // //     // println!("After overlapping insert: {:?}", lock.read().unwrap());
    // //
    // //     let v = r.get_and(&k.clone(), |rs| { rs.iter().any(|r| *r == "x".to_string()) }, uid1.clone()).unwrap();
    // //     assert_eq!(v, true);
    // //
    // //     let v = r.get_and(&k.clone(), |rs| { rs.iter().any(|r| *r == "x2".to_string()) }, uid2.clone()).unwrap();
    // //     assert_eq!(v, true);
    // //
    // //     w.remove(k.clone(), uid1.clone());
    // //     // println!("After remove: {:?}", lock.read().unwrap());
    // //
    // //     let v = r.get_and(&k.clone(), |rs| { false }, uid1.clone());
    // //     // println!("V: {:?}", v);
    // //     match v {
    // //         Some(val) => assert_eq!(val, false),
    // //         None => {}
    // //     };
    // //
    // //     w.remove(k.clone(), uid2.clone());
    // //     // println!("After user specific remove {:?}", lock.read().unwrap());
    // //
    // //     w.remove_user(uid1);
    // //     // println!("After removing u1 {:?}", lock.read().unwrap());
    // //
    // //     w.remove_user(uid2);
    // //     // println!("After removing u2 {:?}", lock.read().unwrap());
    // // }
    //
    // #[test]
    // fn absorb_negative_later() {
    //     let a = vec![1.into(), "a".into()];
    //     let b = vec![1.into(), "b".into()];
    //
    //     let (r, mut w) = new(2, &[0]);
    //     w.add(vec![Record::Positive(a.clone())]);
    //     w.add(vec![Record::Positive(b.clone())]);
    //     w.swap();
    //     w.add(vec![Record::Negative(a.clone())]);
    //     w.swap();
    //
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    // }
    //
    // #[test]
    // fn absorb_multi() {
    //     let a = vec![1.into(), "a".into()];
    //     let b = vec![1.into(), "b".into()];
    //     let c = vec![1.into(), "c".into()];
    //
    //     let (r, mut w) = new(2, &[0]);
    //     w.add(vec![
    //         Record::Positive(a.clone()),
    //         Record::Positive(b.clone()),
    //     ]);
    //     w.swap();
    //
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(2));
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == a[0] && r[1] == a[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    //
    //     w.add(vec![
    //         Record::Negative(a.clone()),
    //         Record::Positive(c.clone()),
    //         Record::Negative(c.clone()),
    //     ]);
    //     w.swap();
    //
    //     assert_eq!(r.try_find_and(&a[0..1], |rs| rs.len()).unwrap().0, Some(1));
    //     assert!(
    //         r.try_find_and(&a[0..1], |rs| rs
    //             .iter()
    //             .any(|r| r[0] == b[0] && r[1] == b[1])).unwrap()
    //         .0
    //         .unwrap()
    //     );
    // }
}
