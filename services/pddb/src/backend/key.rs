use crate::api::*;
use super::*;

use core::cell::RefCell;
use std::num::NonZeroU32;
use std::rc::Rc;
use core::ops::{Deref, DerefMut};
use core::mem::size_of;
use std::convert::TryInto;
use aes_gcm_siv::{Aes256GcmSiv, Nonce};
use aes_gcm_siv::aead::{Aead, Payload};
use std::iter::IntoIterator;
use std::collections::HashMap;
use std::io::{Result, Error, ErrorKind};
use std::cmp::Ordering;
use bitfield::bitfield;

bitfield! {
    #[derive(Copy, Clone, PartialEq, Eq)]
    pub struct KeyFlags(u32);
    impl Debug;
    pub valid, set_valid: 0;
}

/// On-disk representation of the Key. Note that the storage on disk is mis-aligned, so
/// any deserialization must essentially come with a copy step to line up the record.
#[repr(C, align(8))]
pub(crate) struct KeyDescriptor {
    /// virtual address of the key's start
    pub(crate) start: u64,
    /// length of the key's stored data
    pub(crate) len: u64,
    /// amount of space reserved for the key. Must be >= len.
    pub(crate) reserved: u64,
    /// Reserved for flags on the record entry
    pub(crate) flags: KeyFlags,
    /// Access count to the key
    pub(crate) age: u32,
    /// Name. Length should pad out the record to exactly 127 bytes.
    pub(crate) name: [u8; KEY_NAME_LEN],
}
impl Default for KeyDescriptor {
    fn default() -> Self {
        KeyDescriptor {
            start: 0,
            len: 0,
            reserved: 0,
            flags: KeyFlags(0),
            age: 0,
            name: [0; KEY_NAME_LEN],
        }
    }
}
impl Deref for KeyDescriptor {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const KeyDescriptor as *const u8, core::mem::size_of::<KeyDescriptor>())
                as &[u8]
        }
    }
}
impl DerefMut for KeyDescriptor {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self as *mut KeyDescriptor as *mut u8, core::mem::size_of::<KeyDescriptor>())
                as &mut [u8]
        }
    }
}

pub(crate) struct KeyCacheEntry {
    pub(crate) start: u64,
    pub(crate) len: u64,
    pub(crate) reserved: u64,
    pub(crate) flags: KeyFlags,
    pub(crate) age: u32,
    /// the current on-disk index of the KeyCacheEntry item, enumerated as "0" being the Dict descriptor and "1" being the first valid key
    pub(crate) descriptor_index: NonZeroU32,
    /// indicates if the descriptor cache entry is currently synchronized with what's on disk. Does not imply anything about the data,
    /// but if the `data` field is None then there is nothing to in cache to be dirtied.
    pub(crate) clean: bool,
    /// if Some, contains the keys data contents. if None, you must refer to the disk contents to retrieve it.
    pub(crate) data: Option<KeyCacheData>,
}
impl KeyCacheEntry {
    /// Given a base offset of the dictionary containing the key, compute the starting VirtAddr of the key itself.
    pub(crate) fn descriptor_vaddr(&self, dict_offset: VirtAddr) -> VirtAddr {
        VirtAddr::new(dict_offset.get() + ((self.descriptor_index.get() as u64 + 1) * DK_STRIDE as u64)).unwrap()
    }
    /// Computes the modular position of the KeyDescriptor within a vpage.
    pub(crate) fn descriptor_modulus(&self) -> usize {
        (self.descriptor_index.get() as usize + 1) % (VPAGE_SIZE / DK_STRIDE)
    }
    /// Computes the vpage offset as measured from the start of the dictionary storage region
    pub(crate) fn descriptor_vpage_num(&self) -> usize {
        (self.descriptor_index.get() as usize + 1) / (VPAGE_SIZE / DK_STRIDE)
    }
}

pub (crate) enum KeyCacheData {
    Small(KeySmallData),
    // the "Medium" type has a region reserved for it, but we haven't coded a handler for it.
    Large(KeyLargeData),
}
/// Small data is optimized for low overhead, and always represent a complete copy of the data.
pub(crate) struct KeySmallData {
    pub clean: bool,
    pub(crate) data: Vec::<u8>,
}
/// This can hold just a portion of a large key's data. For now, we now essentially manually
/// encode a sub-slice in parts, but, later on we could get more clever and start to cache
/// multiple disjoint portions of a large key's data...
pub(crate) struct KeyLargeData {
    pub clean: bool,
    pub(crate) start: u64,
    pub(crate) data: Vec::<u8>,
}

pub(crate) const SMALL_CAPACITY: usize = VPAGE_SIZE;
/// A storage pool for data that is strictly smaller than one VPAGE. These element are serialized
/// and stored inside the "small data pool" area.
pub(crate) struct KeySmallPool {
    // location of data within the Small memory region. Index is in units of SMALL_CAPACITY. (this should be encoded in the vector position)
    //pub(crate) index: u32,
    /// list of data actually stored within the pool - resolve against `keys` HashMap.
    pub(crate) contents: Vec::<String>,
    /// keeps track of the available space within the pool, avoiding an expensive lookup every time we want to query the available space
    pub(crate) avail: u16,
    pub(crate) clean: bool,
}
impl KeySmallPool {
    pub(crate) fn new() -> KeySmallPool {
        KeySmallPool {
            contents: Vec::<String>::new(),
            avail: SMALL_CAPACITY as u16,
            clean: true,
        }
    }
    /// Tries to fit the key into the current pool. If there isn't enough space, the function returns false.
    /// If `capacity_hint` is provided, it tries to reserve this amount of capacity for the key, even if the
    /// data provided is smaller than the hint.
    pub(crate) fn try_insert_key(&self, data: &KeyCacheEntry, capacity_hint: Option<usize>) -> bool {
        // 1. check if the key will fit
        // 2. if so, add its size to the used pool and contents
        // 3. if not, return false
        false
    }
    /// Tries to update the key in the pool with the new data. If it doesn't fit,
    /// returns Some(key_name). Else it returns None. Note that the data is immediately
    /// evicted if it doesn't fit -- the caller must then find a new pool to place the
    /// evicted key in.
    pub(crate) fn try_update_key(&self, data: &KeyCacheEntry, capacity_hint: Option<usize>) -> Option<String> {
        None
    }
    pub(crate) fn delete_key(&self, data: &KeyCacheEntry) {
        // 1. remove the contents string
        // 2. update the used tracking information
    }
    /// Returns the amount of space available in the pool
    pub(crate) fn free(&self) -> usize {
        self.avail as usize
    }
}
/// a bookkeeping structrue to put into a max-heap to figure out who has the most available space
#[derive(Eq)]
pub(crate) struct KeySmallPoolOrd {
    pub(crate) avail: u16,
    pub(crate) index: usize,
}
// only compare based on the amount of data used
impl Ord for KeySmallPoolOrd {
    fn cmp(&self, other: &Self) -> Ordering {
        self.avail.cmp(&other.avail)
    }
}
impl PartialOrd for KeySmallPoolOrd {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for KeySmallPoolOrd {
    fn eq(&self, other: &Self) -> bool {
        self.avail == other.avail
    }
}
