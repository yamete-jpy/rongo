//! The header sort: keys read from raw BSON, in MongoDB's sort order.

use std::sync::Arc;

use rongo_core::bson::{Bson, Decimal128, Document, RawBson, RawBsonRef, RawDocument, RawDocumentBuf};

use super::drill::index;

/// A sort made here, from a column header.
pub(super) struct LocalSort {
    pub(super) path: String,
    pub(super) dir: i32,
    /// The documents in hand in this order, by index: a static result's
    /// documents, which `fetch` pages through, or a query's page as it
    /// arrived (the order `rows` shows it in).
    pub(super) order: Arc<[usize]>,
}

/// Sort keys read for one path, one per document in hand.
pub(super) struct SortKeys {
    pub(super) path: String,
    /// The documents they were read from: a query's page or a Collection
    /// History preview. Empty for a static result, whose documents never
    /// change.
    pub(super) from: std::sync::Weak<Vec<RawDocumentBuf>>,
    pub(super) keys: Arc<[SortKey]>,
}

/// The value at a dotted path; an index (`drill::index`) steps into an
/// array.
pub(super) fn lookup<'a>(doc: &'a Document, path: &str) -> Option<&'a Bson> {
    let mut parts = path.split('.');
    let mut v = doc.get(parts.next()?)?;
    for p in parts {
        v = match v {
            Bson::Document(d) => d.get(p)?,
            Bson::Array(a) => a.get(index(p)?)?,
            _ => return None,
        };
    }
    Some(v)
}

/// The value at a dotted path of a raw document, found as `lookup` finds it
/// (a number steps into an array), reading only the elements on the way.
pub(super) fn lookup_raw<'a>(doc: &'a RawDocument, path: &str) -> Option<RawBsonRef<'a>> {
    let mut parts = path.split('.');
    let mut v = doc.get(parts.next()?).ok()??;
    for p in parts {
        v = match v {
            RawBsonRef::Document(d) => d.get(p).ok()??,
            RawBsonRef::Array(a) => a.get(index(p)?).ok()??,
            _ => return None,
        };
    }
    Some(v)
}

/// A page of a static result: which of its documents are on it, in the
/// result's own order (so the columns come out as they do unsorted), and
/// the rows' order over them. Rows `start..end` of `sorted`, the result's
/// documents in a header sort's order (`None`: unsorted).
pub(super) fn page_order(sorted: Option<&[usize]>, start: usize, end: usize) -> (Vec<usize>, Option<Arc<[usize]>>) {
    let Some(sorted) = sorted else { return ((start..end).collect(), None) };
    let shown = &sorted[start..end];
    let mut own = shown.to_vec();
    own.sort_unstable();
    let order = shown.iter().map(|i| own.binary_search(i).unwrap_or_default()).collect();
    (own, Some(order))
}

/// Keys as read, ready to sort. A column that mixes doubles and decimals
/// compares them as decimals (as MongoDB does), converted here once rather
/// than in every comparison.
pub(super) fn sort_keys(keys: impl Iterator<Item = SortKey>) -> Arc<[SortKey]> {
    let mut keys: Vec<SortKey> = keys.collect();
    if keys.iter().any(|k| matches!(k, SortKey::Num(Num::Decimal(_)))) {
        for k in &mut keys {
            if let SortKey::Num(n @ Num::Double(_)) = k {
                *n = Num::Decimal(n.decimal());
            }
        }
    }
    keys.into()
}

/// The order of the rows for `keys` (`dir` < 0: descending): their indexes,
/// sorted by key, equal keys in the order they came.
pub(super) fn sort_order(keys: &[SortKey], dir: i32) -> Arc<[usize]> {
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        let o = keys[a].cmp(&keys[b]);
        (if dir < 0 { o.reverse() } else { o }).then(a.cmp(&b))
    });
    order.into()
}

/// A value as MongoDB's sort order sees it, read once per document so that
/// sorting compares no BSON. The variants are in that order; a missing
/// field sorts as null. Arrays compare element by element (the server uses
/// their smallest or largest element), which is close enough for results
/// already in hand.
#[derive(Clone, Debug)]
pub(super) enum SortKey {
    MinKey,
    Null,
    Num(Num),
    Str(Box<str>),
    Doc(Box<[(Box<str>, SortKey)]>),
    Array(Box<[SortKey]>),
    /// Subtype and bytes.
    Binary(u8, Box<[u8]>),
    ObjectId([u8; 12]),
    Bool(bool),
    Date(i64),
    Timestamp(u32, u32),
    /// Pattern and options.
    Regex(Box<str>, Box<str>),
    DbPointer,
    Code(Box<str>),
    CodeWithScope(Box<str>),
    MaxKey,
}

impl SortKey {
    /// The key of a value in raw BSON (`None`: the field is missing).
    pub(super) fn of(v: Option<RawBsonRef>) -> SortKey {
        use RawBsonRef as R;
        let Some(v) = v else { return SortKey::Null };
        match v {
            R::MinKey => SortKey::MinKey,
            R::Null | R::Undefined => SortKey::Null,
            R::Int32(n) => SortKey::Num(Num::Int(n.into())),
            R::Int64(n) => SortKey::Num(Num::Int(n)),
            R::Double(f) => SortKey::Num(Num::Double(f)),
            R::Decimal128(d) => SortKey::Num(Num::Decimal(Decimal::from_decimal128(d))),
            R::String(s) | R::Symbol(s) => SortKey::Str(s.into()),
            R::Document(d) => SortKey::Doc(d.iter().flatten().map(|(k, v)| (k.as_str().into(), SortKey::of(Some(v)))).collect()),
            R::Array(a) => SortKey::Array(a.into_iter().flatten().map(|v| SortKey::of(Some(v))).collect()),
            R::Binary(b) => SortKey::Binary(b.subtype.into(), b.bytes.into()),
            R::ObjectId(id) => SortKey::ObjectId(id.bytes()),
            R::Boolean(b) => SortKey::Bool(b),
            R::DateTime(t) => SortKey::Date(t.timestamp_millis()),
            R::Timestamp(t) => SortKey::Timestamp(t.time, t.increment),
            R::RegularExpression(r) => SortKey::Regex(r.pattern.as_str().into(), r.options.as_str().into()),
            R::DbPointer(_) => SortKey::DbPointer,
            R::JavaScriptCode(c) => SortKey::Code(c.into()),
            R::JavaScriptCodeWithScope(c) => SortKey::CodeWithScope(c.code.into()),
            R::MaxKey => SortKey::MaxKey,
        }
    }

    /// The key of a parsed value (a static result's), read the same way.
    pub(super) fn of_bson(v: Option<&Bson>) -> SortKey {
        match v.cloned().map(RawBson::try_from) {
            Some(Ok(raw)) => SortKey::of(Some(raw.as_raw_bson_ref())),
            _ => SortKey::Null,
        }
    }

    fn rank(&self) -> u8 {
        match self {
            SortKey::MinKey => 0,
            SortKey::Null => 1,
            SortKey::Num(_) => 2,
            SortKey::Str(_) => 3,
            SortKey::Doc(_) => 4,
            SortKey::Array(_) => 5,
            SortKey::Binary(..) => 6,
            SortKey::ObjectId(_) => 7,
            SortKey::Bool(_) => 8,
            SortKey::Date(_) => 9,
            SortKey::Timestamp(..) => 10,
            SortKey::Regex(..) => 11,
            SortKey::DbPointer => 12,
            SortKey::Code(_) => 13,
            SortKey::CodeWithScope(_) => 14,
            SortKey::MaxKey => 15,
        }
    }
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (SortKey::Num(a), SortKey::Num(b)) => a.compare(b),
            (SortKey::Str(a), SortKey::Str(b)) | (SortKey::Code(a), SortKey::Code(b)) | (SortKey::CodeWithScope(a), SortKey::CodeWithScope(b)) => a.cmp(b),
            // Field by field: the value's type, then the name, then the
            // value; a document that runs out first sorts first.
            (SortKey::Doc(a), SortKey::Doc(b)) => a
                .iter()
                .zip(b.iter())
                .map(|((ka, va), (kb, vb))| va.rank().cmp(&vb.rank()).then_with(|| ka.cmp(kb)).then_with(|| va.cmp(vb)))
                .find(|o| o.is_ne())
                .unwrap_or_else(|| a.len().cmp(&b.len())),
            (SortKey::Array(a), SortKey::Array(b)) => a.iter().cmp(b.iter()),
            (SortKey::Binary(ta, a), SortKey::Binary(tb, b)) => a.len().cmp(&b.len()).then(ta.cmp(tb)).then_with(|| a.cmp(b)),
            (SortKey::ObjectId(a), SortKey::ObjectId(b)) => a.cmp(b),
            (SortKey::Bool(a), SortKey::Bool(b)) => a.cmp(b),
            (SortKey::Date(a), SortKey::Date(b)) => a.cmp(b),
            (SortKey::Timestamp(ta, ia), SortKey::Timestamp(tb, ib)) => (ta, ia).cmp(&(tb, ib)),
            (SortKey::Regex(pa, oa), SortKey::Regex(pb, ob)) => (pa, oa).cmp(&(pb, ob)),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SortKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for SortKey {}

/// A number as it sorts: every numeric type on one scale, compared exactly.
#[derive(Clone, Copy, Debug)]
pub(super) enum Num {
    Int(i64),
    Double(f64),
    Decimal(Decimal),
}

impl Num {
    fn compare(&self, other: &Num) -> std::cmp::Ordering {
        match (*self, *other) {
            (Num::Int(a), Num::Int(b)) => a.cmp(&b),
            (Num::Double(a), Num::Double(b)) => compare_doubles(a, b),
            (Num::Int(a), Num::Double(b)) => compare_int_double(a, b),
            (Num::Double(a), Num::Int(b)) => compare_int_double(b, a).reverse(),
            (a, b) => a.decimal().cmp(&b.decimal()),
        }
    }

    /// As a decimal, exactly: an integer, or a double at its exact value
    /// (how MongoDB compares a double with a decimal).
    fn decimal(self) -> Decimal {
        match self {
            Num::Int(n) => Decimal::finite(n < 0, n.unsigned_abs().into(), 0),
            Num::Double(f) => Decimal::from_f64(f),
            Num::Decimal(d) => d,
        }
    }
}

/// NaN first (MongoDB's order), equal to itself; -0 equals 0.
fn compare_doubles(a: f64, b: f64) -> std::cmp::Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// An integer against a double, exactly (`as f64` rounds past 2^53).
fn compare_int_double(i: i64, d: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // 2^63, exactly: every double from there up is above every i64.
    const TWO_63: f64 = 9_223_372_036_854_775_808.0;
    if d.is_nan() || d < -TWO_63 {
        return Ordering::Greater;
    }
    if d >= TWO_63 {
        return Ordering::Less;
    }
    // In range, the whole part converts exactly; the fraction decides a tie.
    let whole = d.trunc();
    i.cmp(&(whole as i64)).then_with(|| 0.0.partial_cmp(&(d - whole)).unwrap_or(Ordering::Equal))
}

/// A decimal in a form that compares exactly: its class (NaN first, then
/// -∞, negative, zero, positive, +∞) and, for a finite value, the exponent
/// of its first digit, its first 34 digits (scaled to 34 of them) and
/// whether any digit after those is not zero (`above`: only a double's
/// exact value has more than 34).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Decimal {
    class: i8,
    exp: i32,
    digits: u128,
    above: bool,
}

/// The base of `Decimal::from_f64`'s limbs: 19 decimal digits each.
const TEN_19: u128 = 10_000_000_000_000_000_000;

impl Decimal {
    const NAN: Decimal = Decimal { class: -3, exp: 0, digits: 0, above: false };

    fn infinity(negative: bool) -> Self {
        Decimal { class: if negative { -2 } else { 2 }, exp: 0, digits: 0, above: false }
    }

    /// ±`coef` × 10^`exp`, for `coef` of at most 34 digits.
    fn finite(negative: bool, coef: u128, exp: i32) -> Self {
        if coef == 0 {
            return Decimal { class: 0, exp: 0, digits: 0, above: false };
        }
        let n = coef.ilog10() as i32 + 1;
        Decimal { class: if negative { -1 } else { 1 }, exp: exp + n - 1, digits: coef * 10u128.pow((34 - n).max(0) as u32), above: false }
    }

    /// From the IEEE 754 BID encoding, with no text in between.
    fn from_decimal128(d: Decimal128) -> Self {
        let bits = u128::from_le_bytes(d.bytes());
        let negative = bits >> 127 == 1;
        match (bits >> 122) & 0x1f {
            0x1f => return Self::NAN,
            0x1e => return Self::infinity(negative),
            _ => {}
        }
        // A 14-bit exponent, then the coefficient. After "11" an implied
        // "100" makes the coefficient too big for 34 digits: that reads as 0.
        let (exp, coef) = if (bits >> 125) & 0b11 == 0b11 { ((bits >> 111) & 0x3fff, 0) } else { ((bits >> 113) & 0x3fff, bits & ((1 << 113) - 1)) };
        let coef = if coef < 10u128.pow(34) { coef } else { 0 };
        Self::finite(negative, coef, exp as i32 - 6176)
    }

    /// A double at its exact value. A finite double is m × 2^e: a whole
    /// number when e ≥ 0, else m × 5^-e digits with -e of them after the
    /// point, so it has finitely many digits (up to 767). They are worked
    /// out in limbs of 19 decimal digits; the first 34 are kept, and
    /// whether any after them is not zero.
    fn from_f64(f: f64) -> Self {
        if f.is_nan() {
            return Self::NAN;
        }
        if f.is_infinite() {
            return Self::infinity(f < 0.0);
        }
        let bits = f.to_bits();
        let (biased, fraction) = ((bits >> 52) & 0x7ff, bits & ((1 << 52) - 1));
        let (m, e) = if biased == 0 { (fraction, -1074) } else { (fraction | 1 << 52, biased as i32 - 1075) };
        if m == 0 {
            return Self::finite(false, 0, 0);
        }
        let tz = m.trailing_zeros();
        let (m, e) = (m >> tz, e + tz as i32);
        // Least significant limb first; m < 2^53 fits one.
        let mut limbs: Vec<u64> = vec![m];
        let (base, chunk, point) = if e >= 0 { (2u64, 60, 0) } else { (5, 27, -e) };
        let mut left = e.unsigned_abs();
        while left > 0 {
            let k = left.min(chunk);
            let factor = u128::from(base.pow(k));
            let mut carry = 0u128;
            for limb in &mut limbs {
                let v = u128::from(*limb) * factor + carry;
                *limb = (v % TEN_19) as u64;
                carry = v / TEN_19;
            }
            while carry > 0 {
                limbs.push((carry % TEN_19) as u64);
                carry /= TEN_19;
            }
            left -= k;
        }
        // The leading digits, enough for 34; the limbs below them are all digits after those.
        let mut low = limbs.len() - 1;
        let mut text = limbs[low].to_string();
        while text.len() < 34 && low > 0 {
            low -= 1;
            text.push_str(&format!("{:019}", limbs[low]));
        }
        let len = (text.len() + 19 * low) as i32;
        let n = text.len().min(34);
        let above = text[n..].bytes().any(|b| b != b'0') || limbs[..low].iter().any(|&l| l != 0);
        let coef: u128 = text[..n].parse().unwrap_or(0);
        Decimal { above, ..Self::finite(f < 0.0, coef, len - n as i32 - point) }
    }
}

impl Ord for Decimal {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let size = || (self.exp, self.digits, self.above).cmp(&(other.exp, other.digits, other.above));
        self.class.cmp(&other.class).then_with(|| match self.class {
            1 => size(),
            -1 => size().reverse(),
            _ => std::cmp::Ordering::Equal,
        })
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use rongo_core::bson::doc;
    use rongo_core::{ConnId, FindSpec, Namespace};

    use super::*;
    use crate::result_view::{ResultView, Source};

    /// Two values (`None`: a missing field) in the header sort's order.
    fn compare(a: Option<&Bson>, b: Option<&Bson>) -> std::cmp::Ordering {
        SortKey::of_bson(a).cmp(&SortKey::of_bson(b))
    }

    fn order(a: &Bson, b: &Bson) -> std::cmp::Ordering {
        compare(Some(a), Some(b))
    }

    fn dec(s: &str) -> Bson {
        Bson::Decimal128(s.parse().unwrap())
    }

    /// Types sort in MongoDB's order, lowest first; a missing field sorts
    /// with null.
    #[test]
    fn sort_ranks_types_as_mongodb_does() {
        use rongo_core::bson::{Binary, DateTime, Regex, Timestamp, oid::ObjectId, spec::BinarySubtype};
        use std::cmp::Ordering::{Equal, Less};
        let ranked = [
            Bson::MinKey,
            Bson::Null,
            Bson::Int32(5),
            Bson::String("a".into()),
            Bson::Document(doc! { "a": 1 }),
            Bson::Array(vec![]),
            Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: vec![] }),
            Bson::ObjectId(ObjectId::new()),
            Bson::Boolean(false),
            Bson::DateTime(DateTime::from_millis(0)),
            Bson::Timestamp(Timestamp { time: 0, increment: 0 }),
            Bson::RegularExpression(Regex { pattern: "a".try_into().unwrap(), options: "".try_into().unwrap() }),
            Bson::MaxKey,
        ];
        for w in ranked.windows(2) {
            assert_eq!(order(&w[0], &w[1]), Less, "{:?} before {:?}", w[0], w[1]);
        }
        assert_eq!(compare(None, Some(&Bson::Null)), Equal);
        assert_eq!(compare(None, Some(&Bson::Int32(0))), Less);
        assert_eq!(order(&Bson::Symbol("b".into()), &Bson::String("a".into())), std::cmp::Ordering::Greater, "a symbol is a string");
    }

    /// Integers compare exactly, also past 2^53 and against doubles.
    #[test]
    fn sort_compares_integers_exactly() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        assert_eq!(order(&Bson::Int64(9_007_199_254_740_993), &Bson::Int64(9_007_199_254_740_992)), Greater);
        assert_eq!(order(&Bson::Int64(9_007_199_254_740_993), &Bson::Double(9_007_199_254_740_992.0)), Greater);
        assert_eq!(order(&Bson::Double(9_007_199_254_740_992.0), &Bson::Int64(9_007_199_254_740_993)), Less);
        assert_eq!(order(&Bson::Int64(i64::MAX), &Bson::Double(9_223_372_036_854_775_807.0)), Less, "i64::MAX is just under 2^63");
        assert_eq!(order(&Bson::Int64(i64::MIN), &Bson::Double(-9_223_372_036_854_775_808.0)), Equal);
        assert_eq!(order(&Bson::Int32(1), &Bson::Double(1.0)), Equal);
        assert_eq!(order(&Bson::Int32(1), &Bson::Int64(1)), Equal);
        assert_eq!(order(&Bson::Int32(2), &Bson::Double(1.5)), Greater);
        assert_eq!(order(&Bson::Int32(-1), &Bson::Double(-1.5)), Greater);
        assert_eq!(order(&Bson::Int64(i64::MIN), &Bson::Double(f64::NEG_INFINITY)), Greater);
        assert_eq!(order(&Bson::Int64(i64::MAX), &Bson::Double(f64::INFINITY)), Less);
    }

    /// NaN sorts before every number (MongoDB's order), equal to itself;
    /// -0 is 0. A page sorted ascending shows NaN first.
    #[test]
    fn sort_puts_nan_before_numbers() {
        use std::cmp::Ordering::{Equal, Less};
        assert_eq!(order(&Bson::Double(f64::NAN), &Bson::Int32(1)), Less);
        assert_eq!(order(&Bson::Double(f64::NAN), &Bson::Double(f64::NEG_INFINITY)), Less);
        assert_eq!(order(&Bson::Double(f64::NAN), &Bson::Double(-f64::NAN)), Equal);
        assert_eq!(order(&Bson::Double(-0.0), &Bson::Int32(0)), Equal);

        let page = [doc! { "_id": 1, "v": 1 }, doc! { "_id": 2, "v": f64::NAN }].iter().map(|d| RawDocumentBuf::try_from(d).unwrap()).collect();
        let mut v = ResultView::new(1, "t", Source::Find { conn: ConnId(uuid::Uuid::nil()), spec: FindSpec::all(Namespace::new("d", "c"), 50), max: None });
        v.paging.needs_fetch = false;
        v.paging.loading = Some(1);
        v.on_page(1, page, false, 1);
        v.sort_by("v".into(), Some(1));
        assert_eq!(v.rows.doc(0).and_then(|d| d.get("_id")), Some(&Bson::Int32(2)), "NaN first");
    }

    /// Decimal128 compares exactly, with itself and with the other numbers.
    #[test]
    fn sort_compares_decimals_exactly() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        assert_eq!(order(&dec("0.10000000000000000001"), &dec("0.1")), Greater);
        assert_eq!(order(&dec("1234567890123456789012345678901234"), &dec("1234567890123456789012345678901233")), Greater);
        assert_eq!(order(&dec("9007199254740993"), &Bson::Int64(9_007_199_254_740_992)), Greater);
        assert_eq!(order(&dec("1.000"), &Bson::Int32(1)), Equal);
        assert_eq!(order(&dec("1E+3"), &dec("1000")), Equal);
        assert_eq!(order(&dec("1E+3"), &Bson::Double(999.5)), Greater);
        assert_eq!(order(&dec("0.1"), &Bson::Double(0.1)), Less, "the double 0.1 is a little over a tenth");
        assert_eq!(order(&dec("-0"), &Bson::Double(0.0)), Equal);
        assert_eq!(order(&dec("-12.5"), &Bson::Int32(-12)), Less);
        assert_eq!(order(&dec("-12.5"), &dec("-12.25")), Less);
        assert_eq!(order(&dec("NaN"), &Bson::Double(f64::NEG_INFINITY)), Less);
        assert_eq!(order(&dec("NaN"), &Bson::Double(f64::NAN)), Equal);
        assert_eq!(order(&dec("Infinity"), &Bson::Int64(i64::MAX)), Greater);
        assert_eq!(order(&dec("-Infinity"), &Bson::Double(f64::NEG_INFINITY)), Equal);
    }

    /// A double meets a decimal at its exact value, as MongoDB 7 compares
    /// them (every finite double has a finite decimal expansion): equal only
    /// when the values are, never because the double rounds to the
    /// decimal's 34 digits.
    #[test]
    fn doubles_meet_decimals_at_their_exact_value() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        let values = [Bson::Double(0.1), dec("0.1"), dec("0.1000000000000000055511151231257827")];
        let keys = sort_keys(values.iter().map(|v| SortKey::of_bson(Some(v))));
        assert_eq!(&*sort_order(&keys, 1), [1, 2, 0], "Decimal 0.1, Decimal 0.1000…827, Double 0.1 (0.1000…827021…)");
        assert_eq!(&*sort_order(&keys, -1), [0, 2, 1]);
        assert_eq!(order(&Bson::Double(0.5), &dec("0.5")), Equal);
        assert_eq!(order(&Bson::Double(0.3), &dec("0.2999999999999999888977697537484346")), Less, "0.3 rounds up to it at 34 digits, but is less");
        assert_eq!(order(&Bson::Double(-0.1), &dec("-0.1000000000000000055511151231257827")), Less);
        assert_eq!(order(&Bson::Double(1e300), &dec("1.000000000000000052504760255204420E+300")), Greater);
        assert_eq!(order(&Bson::Double(5e-324), &dec("4.940656458412465441765687928682214E-324")), Less, "the smallest double, rounded up at 34 digits");
        assert_eq!(order(&Bson::Double(5e-324), &dec("4.940656458412465441765687928682213E-324")), Greater);
        assert_eq!(order(&Bson::Double(f64::MAX), &dec("1.797693134862315708145274237317043E+308")), Greater);
        assert_eq!(order(&Bson::Double(f64::MAX), &dec("1.797693134862315708145274237317044E+308")), Less);
        assert_eq!(order(&Bson::Double(9_007_199_254_740_993.0), &dec("9007199254740992")), Equal, "that double is 2^53");
        assert_eq!(order(&Bson::Double(1e22), &dec("1E+22")), Equal, "a power of ten a double holds exactly");
        // Read as decimals, doubles of every size keep their own order.
        let mut x = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            f64::from_bits(x)
        };
        let doubles: Vec<f64> = std::iter::repeat_with(&mut next).filter(|f| !f.is_nan()).take(4000).chain([0.0, -0.0, 5e-324, -5e-324, f64::MAX, f64::MIN, 1.0, 0.1]).collect();
        for w in doubles.windows(2) {
            let (a, b) = (Num::Double(w[0]), Num::Double(w[1]));
            assert_eq!(a.decimal().cmp(&b.decimal()), compare_doubles(w[0], w[1]), "{:e} / {:e}", w[0], w[1]);
        }
        // And each is what Rust's own exact formatting gives (at most 767 digits).
        for &f in doubles.iter().rev().take(600) {
            let text = format!("{:.800e}", f.abs());
            let (digits, exp) = text.split_once('e').unwrap();
            let digits = digits.replace('.', "");
            let coef: u128 = digits[..34].parse().unwrap();
            let want = Decimal { above: digits[34..].bytes().any(|b| b != b'0'), ..Decimal::finite(f < 0.0, coef, exp.parse::<i32>().unwrap() - 33) };
            assert_eq!(Decimal::from_f64(f), want, "{f:e}");
        }
    }

    /// Embedded documents compare field by field: the type, then the name,
    /// then the value; one that runs out first sorts first.
    #[test]
    fn sort_compares_documents_by_type_name_then_value() {
        use std::cmp::Ordering::{Greater, Less};
        assert_eq!(order(&Bson::Document(doc! { "a": 2 }), &Bson::Document(doc! { "b": 1 })), Less);
        assert_eq!(order(&Bson::Document(doc! { "b": 1 }), &Bson::Document(doc! { "a": "x" })), Less, "a number before a string, whatever the names");
        assert_eq!(order(&Bson::Document(doc! { "a": 1 }), &Bson::Document(doc! { "a": 1, "b": 0 })), Less);
        assert_eq!(order(&Bson::Document(doc! { "a": 1, "b": 2 }), &Bson::Document(doc! { "a": 1, "b": 1 })), Greater);
    }

    /// A sort key is read from the raw document by walking the path, and
    /// finds what `lookup` finds in the parsed one: through embedded
    /// documents and array indexes, or nothing.
    #[test]
    fn raw_sort_keys_follow_the_path_as_lookup_does() {
        let d = doc! { "a": { "b": [10, { "c": "x" }] }, "n": { "0": 5 }, "s": "t", "arr": [[1, 2], [3]], "dec": dec("1.5") };
        let raw = RawDocumentBuf::try_from(&d).unwrap();
        for path in ["a", "a.b", "a.b.0", "a.b.1.c", "a.b.5", "a.z", "a.b.x", "n.0", "s.0", "arr.1.0", "arr.0", "dec", "missing", "", "a..b"] {
            assert_eq!(lookup_raw(&raw, path).is_some(), lookup(&d, path).is_some(), "{path}");
            assert_eq!(SortKey::of(lookup_raw(&raw, path)), SortKey::of_bson(lookup(&d, path)), "{path}");
        }
        assert_eq!(SortKey::of(lookup_raw(&raw, "a.b.1.c")), SortKey::Str("x".into()));
        // An index is written as MongoDB writes one, as the Table view
        // reads it: `01` and `+1` are no index.
        for path in ["arr.01", "arr.+1", "a.b.+0"] {
            assert!(lookup_raw(&raw, path).is_none() && lookup(&d, path).is_none(), "{path}");
        }
    }

    /// A column mixing doubles and decimals sorts the way pairs compare:
    /// the doubles are read as decimals once, for the whole column.
    #[test]
    fn mixed_number_columns_sort_as_mongodb_does() {
        let values = [Bson::Double(0.1), dec("0.1"), Bson::Int32(0), dec("NaN"), Bson::Double(f64::NEG_INFINITY), Bson::Int64(1), Bson::Double(0.1)];
        let keys = sort_keys(values.iter().map(|v| SortKey::of_bson(Some(v))));
        assert!(keys.iter().all(|k| !matches!(k, SortKey::Num(Num::Double(_)))), "doubles read as decimals");
        assert_eq!(&*sort_order(&keys, 1), [3, 4, 2, 1, 0, 6, 5], "NaN, -∞, 0, 0.1 (decimal), 0.1 (double, a little more), 1; ties in arrival order");
        assert_eq!(&*sort_order(&keys, -1), [5, 0, 6, 1, 2, 4, 3], "descending, ties still in arrival order");
    }

    /// Binary data compares by length, then subtype, then the bytes.
    #[test]
    fn sort_compares_binary_by_length_subtype_then_bytes() {
        use rongo_core::bson::{Binary, spec::BinarySubtype};
        use std::cmp::Ordering::Less;
        let bin = |subtype, bytes: &[u8]| Bson::Binary(Binary { subtype, bytes: bytes.to_vec() });
        assert_eq!(order(&bin(BinarySubtype::Generic, &[9, 9]), &bin(BinarySubtype::Uuid, &[0, 0])), Less);
        assert_eq!(order(&bin(BinarySubtype::Uuid, &[0]), &bin(BinarySubtype::Generic, &[0, 0])), Less);
        assert_eq!(order(&bin(BinarySubtype::Generic, &[1, 2]), &bin(BinarySubtype::Generic, &[1, 3])), Less);
    }
}
