//! Actual signing.

use std::{fmt, io, slice};
use std::iter::FromIterator;
use bytes::Bytes;
use domain_core::{
    CanonicalOrd, Compose, Dname, Record, RecordData, Serial, ToDname
};
use domain_core::iana::{Class, Rtype};
use domain_core::rdata::{Dnskey, Ds, Nsec, Rrsig};
use domain_core::rdata::rfc4034::RtypeBitmap;
use crate::key::SigningKey;


//------------ SortedRecords -------------------------------------------------

/// A collection of resource records sorted for signing.
#[derive(Clone)]
pub struct SortedRecords<N, D> {
    records: Vec<Record<N, D>>,
}

impl<N, D> SortedRecords<N, D> {
    pub fn new() -> Self {
        SortedRecords { records: Vec::new() }
    }

    pub fn insert(
        &mut self,
        record: Record<N, D>
    ) -> Result<(), Record<N, D>>
    where N: ToDname, D: RecordData + CanonicalOrd {
        let idx = self.records.binary_search_by(|stored| {
            stored.canonical_cmp(&record)
        });
        match idx {
            Ok(_) => Err(record),
            Err(idx) => {
                self.records.insert(idx, record);
                Ok(())
            }
        }
    }

    pub fn families(&self) -> RecordsIter<N, D> {
        RecordsIter::new(&self.records)
    }

    pub fn rrsets(&self) -> RrsetIter<N, D> {
        RrsetIter::new(&self.records)
    }

    pub fn find_soa(&self) -> Option<Rrset<N, D>>
    where N: ToDname, D: RecordData {
        for rrset in self.rrsets() {
            if rrset.rtype() == Rtype::Soa {
                return Some(rrset)
            }
        }
        None
    }


    pub fn sign<K: SigningKey>(
        &self,
        apex: &FamilyName<Dname>,
        expiration: Serial,
        inception: Serial,
        key: K
    ) -> Result<Vec<Record<Dname, Rrsig>>, K::Error>
    where N: ToDname, D: RecordData {
        let mut res = Vec::new();
        let mut buf = Vec::new();

        // The owner name of a zone cut if we currently are at or below one.
        let mut cut: Option<FamilyName<Dname>> = None;

        let mut families = self.families();

        // Since the records are ordered, the first family is the apex --
        // we can skip everything before that.
        families.skip_before(apex);
        
        for family in families {
            // If the owner is out of zone, we have moved out of our zone and
            // are done.
            if !family.is_in_zone(apex) {
                break
            }

            // If the family is below a zone cut, we must ignore it.
            if let Some(ref cut) = cut {
                if family.owner().ends_with(cut.owner()) {
                    continue
                }
            }

            // Create an owned, uncompressed family name. We’ll need it later.
            let name = family.family_name().to_name();

            // If this family is the parent side of a zone cut, we keep the
            // family name for later. This also means below that if
            // `cut.is_some()` we are at the parent side of a zone.
            cut = if family.is_zone_cut(apex) {
                Some(name.clone())
            }
            else {
                None
            };

            for rrset in family.rrsets() {
                if cut.is_some() {
                    // If we are at a zone cut, we only sign DS and NSEC
                    // records. NS records we must not sign and everything
                    // else shouldn’t be here, really.
                    if rrset.rtype() != Rtype::Ds
                                             && rrset.rtype() != Rtype::Nsec {
                        continue
                    }
                }
                else {
                    // Otherwise we only ignore RRSIGs.
                    if rrset.rtype() == Rtype::Rrsig {
                        continue
                    }
                }

                // Let’s make a signature!
                let mut rrsig = Record::new(
                    name.owner().clone(),
                    name.class(),
                    rrset.ttl(),
                    Rrsig::new(
                        rrset.rtype(),
                        key.algorithm()?,
                        name.owner().rrsig_label_count(),
                        rrset.ttl(),
                        expiration,
                        inception,
                        key.key_tag()?,
                        apex.owner().clone(),
                        Bytes::new(),
                    )
                );
                buf.clear();
                rrsig.data().compose_canonical(&mut buf);

                for record in rrset.iter() {
                    record.compose_canonical(&mut buf);
                }

                rrsig.data_mut().set_signature(key.sign(&buf)?);
                res.push(rrsig);
            }
        }
        Ok(res)
    }

    pub fn nsecs(
        &self,
        apex: &FamilyName<Dname>,
        ttl: u32
    ) -> Vec<Record<Dname, Nsec<Dname>>>
    where N: ToDname, D: RecordData {
        let mut res = Vec::new();

        // The owner name of a zone cut if we currently are at or below one.
        let mut cut: Option<FamilyName<Dname>> = None;

        let mut families = self.families();

        // Since the records are ordered, the first family is the apex --
        // we can skip everything before that.
        families.skip_before(apex);

        // Because of the next name thing, we need to keep the last NSEC
        // around.
        let mut prev: Option<Record<Dname, Nsec<Dname>>> = None;

        for family in families {
            // If the owner is out of zone, we have moved out of our zone and
            // are done.
            if !family.is_in_zone(apex) {
                break
            }

            // If the family is below a zone cut, we must ignore it.
            if let Some(ref cut) = cut {
                if family.owner().ends_with(cut.owner()) {
                    continue
                }
            }

            // Create an owned, uncompressed family name. We’ll need it later.
            let name = family.family_name().to_name();

            // If this family is the parent side of a zone cut, we keep the
            // family name for later. This also means below that if
            // `cut.is_some()` we are at the parent side of a zone.
            cut = if family.is_zone_cut(apex) {
                Some(name.clone())
            }
            else {
                None
            };

            if let Some(mut nsec) = prev.take() {
                nsec.data_mut().set_next_name(name.owner().clone());
                res.push(nsec);
            }

            let mut bitmap = RtypeBitmap::builder();
            bitmap.add(Rtype::Rrsig); // Assume there’s gonna be an RRSIG.
            for rrset in family.rrsets() {
                bitmap.add(rrset.rtype())
            }

            prev = Some(name.into_record(ttl, Nsec::new(
                Dname::root(),
                bitmap.finalize()
            )))
        }
        if let Some(mut nsec) = prev {
            nsec.data_mut().set_next_name(apex.owner().clone());
            res.push(nsec)
        }
        res
    }

    pub fn write<W>(&self, target: &mut W) -> Result<(), io::Error>
    where N: fmt::Display, D: RecordData + fmt::Display, W: io::Write {
        for record in &self.records {
            writeln!(target, "{}", record)?;
        }
        Ok(())
    }
}

impl<N, D> Default for SortedRecords<N, D> {
    fn default() -> Self {
        Self::new()
    }
}


impl<N, D> From<Vec<Record<N, D>>> for SortedRecords<N, D>
where N: ToDname, D: RecordData + CanonicalOrd {
    fn from(mut src: Vec<Record<N, D>>) -> Self {
        src.sort_by(CanonicalOrd::canonical_cmp);
            SortedRecords { records: src }
    }
}

impl<N, D> FromIterator<Record<N, D>> for SortedRecords<N, D>
where N: ToDname, D: RecordData + CanonicalOrd {
    fn from_iter<T: IntoIterator<Item = Record<N, D>>>(iter: T) -> Self {
        let mut res = Self::new();
        for item in iter {
            let _ = res.insert(item);
        }
        res
    }
}

impl<N, D> Extend<Record<N, D>> for SortedRecords<N, D>
where N: ToDname, D: RecordData + CanonicalOrd {
    fn extend<T: IntoIterator<Item = Record<N, D>>>(&mut self, iter: T) {
        for item in iter {
            let _ = self.insert(item);
        }
    }
}


//------------ Family --------------------------------------------------------

/// A set of records with the same owner name and class.
pub struct Family<'a, N, D> {
    slice: &'a [Record<N, D>],
}

impl<'a, N, D> Family<'a, N, D> {
    fn new(slice: &'a [Record<N, D>]) -> Self {
        Family { slice }
    }

    pub fn owner(&self) -> &N {
        self.slice[0].owner()
    }

    pub fn class(&self) -> Class {
        self.slice[0].class()
    }

    pub fn family_name(&self) -> FamilyName<&N> {
        FamilyName::new(self.owner(), self.class())
    }

    pub fn rrsets(&self) -> FamilyIter<'a, N, D> {
        FamilyIter::new(self.slice)
    }

    pub fn records(&self) -> slice::Iter<'a, Record<N, D>> {
        self.slice.iter()
    }

    pub fn is_zone_cut<NN>(&self, apex: &FamilyName<NN>) -> bool
    where N: ToDname, NN: ToDname, D: RecordData {
        self.family_name().ne(apex)
        && self.records().any(|record| record.rtype() == Rtype::Ns)
    }

    pub fn is_in_zone<NN: ToDname>(&self, apex: &FamilyName<NN>) -> bool
    where N: ToDname {
        self.owner().ends_with(&apex.owner) && self.class() == apex.class
    }
}


//------------ FamilyName ----------------------------------------------------

/// The identifier for a family, i.e., a owner name and class.
#[derive(Clone)]
pub struct FamilyName<N> {
    owner: N,
    class: Class
}

impl<N> FamilyName<N> {
    fn new(owner: N, class: Class) -> Self {
        FamilyName { owner, class }
    }

    pub fn owner(&self) -> &N {
        &self.owner
    }

    pub fn class(&self) -> Class {
        self.class
    }

    pub fn to_name(&self) -> FamilyName<Dname>
    where N: ToDname {
        FamilyName::new(self.owner.to_name(), self.class)
    }

    pub fn into_record<D>(self, ttl: u32, data: D) -> Record<N, D> {
        Record::new(self.owner, self.class, ttl, data)
    }

    pub fn dnskey<K: SigningKey>(
        &self,
        ttl: u32,
        key: K
    ) -> Result<Record<N, Dnskey>, K::Error>
    where N: Clone {
        key.dnskey().map(|dnskey| self.clone().into_record(ttl, dnskey))
    }

    pub fn ds<K: SigningKey>(
        &self,
        ttl: u32,
        key: K
    ) -> Result<Record<N, Ds>, K::Error>
    where N: ToDname + Clone {
        key.ds(&self.owner).map(|ds| self.clone().into_record(ttl, ds))
    }
}

impl<N: ToDname, NN: ToDname> PartialEq<FamilyName<NN>> for FamilyName<N> {
    fn eq(&self, other: &FamilyName<NN>) -> bool {
        self.owner.name_eq(&other.owner) && self.class == other.class
    }
}

impl<N: ToDname, NN: ToDname, D> PartialEq<Record<NN, D>> for FamilyName<N> {
    fn eq(&self, other: &Record<NN, D>) -> bool {
        self.owner.name_eq(other.owner()) && self.class == other.class()
    }
}


//------------ Rrset ---------------------------------------------------------

/// A set of records with the same owner name, class, and record type.
pub struct Rrset<'a, N, D> {
    slice: &'a [Record<N, D>],
}

impl<'a, N, D> Rrset<'a, N, D> {
    fn new(slice: &'a [Record<N, D>]) -> Self {
        Rrset { slice }
    }

    pub fn owner(&self) -> &N {
        self.slice[0].owner()
    }

    pub fn class(&self) -> Class {
        self.slice[0].class()
    }

    pub fn family_name(&self) -> FamilyName<&N> {
        FamilyName::new(self.owner(), self.class())
    }

    pub fn rtype(&self) -> Rtype
    where D: RecordData {
        self.slice[0].rtype()
    }

    pub fn ttl(&self) -> u32 {
        self.slice[0].ttl()
    }

    pub fn first(&self) -> &Record<N, D> {
        &self.slice[0]
    }

    pub fn iter(&self) -> slice::Iter<'a, Record<N, D>> {
        self.slice.iter()
    }
}


//------------ RecordsIter ---------------------------------------------------

/// An iterator that produces families from sorted records.
pub struct RecordsIter<'a, N, D> {
    slice: &'a [Record<N, D>],
}

impl<'a, N, D> RecordsIter<'a, N, D> {
    fn new(slice: &'a [Record<N, D>]) -> Self {
        RecordsIter { slice }
    }

    pub fn skip_before<NN: ToDname>(&mut self, apex: &FamilyName<NN>)
    where N: ToDname {
        while let Some(first) = self.slice.first() {
            if apex == first {
                break;
            }
            self.slice = &self.slice[1..]
        }
    }
}


impl<'a, N, D> Iterator for RecordsIter<'a, N, D>
where
    N: ToDname + 'a,
    D: RecordData + 'a,
{
    type Item = Family<'a, N, D>;

    fn next(&mut self) -> Option<Self::Item> {
        let first = match self.slice.first() {
            Some(first) => first,
            None => return None,
        };
        let mut end = 1;
        while let Some(record) = self.slice.get(end) {
            if !record.owner().name_eq(first.owner())
                || record.class() != first.class()
            {
                break
            }
            end += 1;
        }
        let (res, slice) = self.slice.split_at(end);
        self.slice = slice;
        Some(Family::new(res))
    }
}


//------------ RrsetIter -----------------------------------------------------

/// An iterator that produces RRsets from sorted records.
pub struct RrsetIter<'a, N, D> {
    slice: &'a [Record<N, D>],
}

impl<'a, N, D> RrsetIter<'a, N, D> {
    fn new(slice: &'a [Record<N, D>]) -> Self {
        RrsetIter { slice }
    }
}

impl<'a, N, D> Iterator for RrsetIter<'a, N, D>
where
    N: ToDname + 'a,
    D: RecordData + 'a,
{
    type Item = Rrset<'a, N, D>;

    fn next(&mut self) -> Option<Self::Item> {
        let first = match self.slice.first() {
            Some(first) => first,
            None => return None,
        };
        let mut end = 1;
        while let Some(record) = self.slice.get(end) {
            if !record.owner().name_eq(first.owner())
                || record.rtype() != first.rtype()
                || record.class() != first.class()
            {
                break
            }
            end += 1;
        }
        let (res, slice) = self.slice.split_at(end);
        self.slice = slice;
        Some(Rrset::new(res))
    }
}


//------------ FamilyIter ----------------------------------------------------

/// An iterator that produces RRsets from a record family.
pub struct FamilyIter<'a, N, D> {
    slice: &'a [Record<N, D>],
}

impl<'a, N, D> FamilyIter<'a, N, D> {
    fn new(slice: &'a [Record<N, D>]) -> Self {
        FamilyIter { slice }
    }
}

impl<'a, N, D> Iterator for FamilyIter<'a, N, D>
where
    N: ToDname + 'a,
    D: RecordData + 'a,
{
    type Item = Rrset<'a, N, D>;

    fn next(&mut self) -> Option<Self::Item> {
        let first = match self.slice.first() {
            Some(first) => first,
            None => return None,
        };
        let mut end = 1;
        while let Some(record) = self.slice.get(end) {
            if record.rtype() != first.rtype() {
                break
            }
            end += 1;
        }
        let (res, slice) = self.slice.split_at(end);
        self.slice = slice;
        Some(Rrset::new(res))
    }
}

