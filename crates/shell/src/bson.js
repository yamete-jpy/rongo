// The BSON types and their Extended JSON codec: all that a detached parse
// (the document editor, the query bar, the dialogs, Paste) evaluates, and
// the first part of every shell run (prelude.js follows it).
// Rust provides one native function: __native(op, json) -> json, which
// returns {"ok": value} or {"err": message, code, codeName}. Values cross
// the boundary as Extended JSON, so BSON types survive the round trip:
// canonical from JS to Rust; from Rust, documents for the script have
// Int32s and finite doubles as bare numbers (runtime.rs `write_js`), which
// JS reads as the same Numbers.
(function () {
"use strict";

// ------------------------------------------------------------ BSON types

class ObjectId {
  constructor(hex) {
    if (hex === undefined) hex = __call("new_oid", {});
    if (hex instanceof ObjectId) hex = hex.hex;
    if (typeof hex !== "string" || !/^[0-9a-fA-F]{24}$/.test(hex)) {
      throw new TypeError("ObjectId() needs a 24 character hex string");
    }
    this.hex = hex.toLowerCase();
  }
  toHexString() { return this.hex; }
  toString() { return this.hex; }
  equals(o) { return o instanceof ObjectId && o.hex === this.hex; }
  getTimestamp() { return new Date(parseInt(this.hex.slice(0, 8), 16) * 1000); }
  toEJSON() { return { $oid: this.hex }; }
  toJSON() { return this.hex; }
}

class Long {
  constructor(v) {
    const s = typeof v === "bigint" ? v.toString() : String(v);
    if (!/^-?\d+$/.test(s)) throw new TypeError("NumberLong() needs an integer, got " + s);
    this.value = s;
  }
  valueOf() { return Number(this.value); }
  toNumber() { return Number(this.value); }
  toString() { return this.value; }
  toEJSON() { return { $numberLong: this.value }; }
  toJSON() { return Number(this.value); }
}

class Int32 {
  constructor(v) {
    const n = Number(v);
    if (!Number.isInteger(n) || n < -2147483648 || n > 2147483647) throw new TypeError("NumberInt() out of range");
    this.value = n;
  }
  valueOf() { return this.value; }
  toEJSON() { return { $numberInt: String(this.value) }; }
}

class Double {
  constructor(v) { this.value = Number(v); }
  valueOf() { return this.value; }
  toEJSON() { return { $numberDouble: __doubleText(this.value) }; }
}

class Decimal128 {
  constructor(s) { this.value = String(s); }
  toString() { return this.value; }
  valueOf() { return Number(this.value); }
  toEJSON() { return { $numberDecimal: this.value }; }
  toJSON() { return this.value; }
}

class Binary {
  constructor(base64, subType) { this.base64 = base64; this.subType = subType || 0; }
  toEJSON() {
    const st = this.subType.toString(16).padStart(2, "0");
    return { $binary: { base64: this.base64, subType: st } };
  }
}

class UUIDValue {
  constructor(s) {
    if (s === undefined) s = __call("new_uuid", {});
    const hex = String(s).replace(/-/g, "").toLowerCase();
    if (!/^[0-9a-f]{32}$/.test(hex)) throw new TypeError("UUID() needs 32 hex digits");
    this.hex = hex;
  }
  toString() {
    const h = this.hex;
    return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
  }
  toEJSON() { return { $uuid: this.hex }; }
}

class Timestamp {
  constructor(t, i) {
    if (typeof t === "object" && t !== null) { i = t.i; t = t.t; }
    this.t = t >>> 0; this.i = (i || 0) >>> 0;
  }
  toEJSON() { return { $timestamp: { t: this.t, i: this.i } }; }
}

class BSONRegExp {
  constructor(pattern, options) { this.pattern = pattern; this.options = options || ""; }
  toEJSON() { return { $regularExpression: { pattern: this.pattern, options: this.options } }; }
}

class Code {
  constructor(code, scope) { this.code = String(code); this.scope = scope; }
  toEJSON() { return this.scope ? { $code: this.code, $scope: __enc(this.scope) } : { $code: this.code }; }
}

// Deprecated BSON types a server can still hold; here so that the editor's
// text reads them back as themselves.
class BSONSymbol {
  constructor(s) { this.value = String(s); }
  toString() { return this.value; }
  valueOf() { return this.value; }
  toEJSON() { return { $symbol: this.value }; }
}

class DBPointer {
  constructor(ns, oid) {
    this.namespace = String(ns);
    this.oid = oid instanceof ObjectId ? oid : new ObjectId(oid);
  }
  toEJSON() { return { $dbPointer: { $ref: this.namespace, $id: this.oid.toEJSON() } }; }
}

const MinKey = Object.freeze({ toEJSON() { return { $minKey: 1 }; } });
const MaxKey = Object.freeze({ toEJSON() { return { $maxKey: 1 }; } });

// Lookup tables for __base64Hex, made on first use.
let __b64Values = null, __hexDigits = null;

// The bytes of base64 text as hex digits (a UUID read from the server), or
// null if it isn't base64. Here rather than a call into Rust per value.
function __base64Hex(text) {
  if (__b64Values === null) {
    __b64Values = new Int8Array(128).fill(-1);
    const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (let i = 0; i < 64; i++) __b64Values[alphabet.charCodeAt(i)] = i;
    __hexDigits = Array.from({ length: 256 }, (_, i) => i.toString(16).padStart(2, "0"));
  }
  let hex = "", acc = 0, bits = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c === 61) break; // "="
    const v = c < 128 ? __b64Values[c] : -1;
    if (v < 0) return null;
    acc = ((acc << 6) | v) & 0xffff;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      hex += __hexDigits[(acc >> bits) & 255];
    }
  }
  return hex;
}

function __doubleText(n) {
  if (Number.isNaN(n)) return "NaN";
  if (n === Infinity) return "Infinity";
  if (n === -Infinity) return "-Infinity";
  // (String(-0) is "0".)
  return Object.is(n, -0) ? "-0" : String(n);
}

// Constructors as mongosh spells them, callable with or without `new`.
globalThis.ObjectId = new Proxy(ObjectId, { apply: (T, _, a) => new T(...a) });
globalThis.ObjectID = globalThis.ObjectId;
globalThis.NumberLong = (v) => new Long(v);
globalThis.Long = new Proxy(Long, { apply: (T, _, a) => new T(...a) });
globalThis.NumberInt = (v) => new Int32(v);
globalThis.Int32 = globalThis.NumberInt;
globalThis.Double = new Proxy(Double, { apply: (T, _, a) => new T(...a) });
globalThis.NumberDecimal = (v) => new Decimal128(v);
globalThis.Decimal128 = new Proxy(Decimal128, { apply: (T, _, a) => new T(...a) });
globalThis.BinData = (subType, base64) => new Binary(base64, subType);
globalThis.UUID = (s) => new UUIDValue(s);
globalThis.Timestamp = new Proxy(Timestamp, { apply: (T, _, a) => new T(...a) });
globalThis.BSONRegExp = (p, o) => new BSONRegExp(p, o);
globalThis.Code = new Proxy(Code, { apply: (T, _, a) => new T(...a) });
globalThis.BSONSymbol = new Proxy(BSONSymbol, { apply: (T, _, a) => new T(...a) });
globalThis.DBPointer = new Proxy(DBPointer, { apply: (T, _, a) => new T(...a) });
globalThis.MinKey = () => MinKey;
globalThis.MaxKey = () => MaxKey;
globalThis.ISODate = function (s) {
  if (s === undefined) return new Date();
  // Like mongosh: a date and time without an offset is UTC (JS would read
  // it as local time), and a space may separate the two.
  if (typeof s === "string") {
    const m = /^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?)$/.exec(s.trim());
    if (m) s = m[1] + "T" + m[2] + "Z";
  }
  const d = new Date(s);
  if (Number.isNaN(d.getTime())) throw new TypeError("invalid ISODate: " + s);
  return d;
};

// ------------------------------------------------------------ EJSON codec

function __enc(v) {
  if (v === null) return null;
  if (v === undefined) return { $undefined: true };
  switch (typeof v) {
    case "number":
      // -0 has no integer form: it stays a Double, sign and all.
      if (Number.isInteger(v) && !Object.is(v, -0)) {
        if (v >= -2147483648 && v <= 2147483647) return { $numberInt: String(v) };
        if (Math.abs(v) <= Number.MAX_SAFE_INTEGER) return { $numberLong: String(v) };
      }
      return { $numberDouble: __doubleText(v) };
    case "string":
    case "boolean":
      return v;
    case "bigint":
      return { $numberLong: v.toString() };
    case "function":
      return { $code: v.toString() };
    case "symbol":
      return String(v);
  }
  if (v instanceof Date) {
    const ms = v.getTime();
    if (Number.isNaN(ms)) throw new TypeError("invalid Date");
    return { $date: { $numberLong: String(ms) } };
  }
  if (v instanceof RegExp) {
    const opts = v.flags.split("").filter((f) => "imsux".includes(f)).sort().join("");
    return { $regularExpression: { pattern: v.source, options: opts } };
  }
  if (typeof v.toEJSON === "function") return v.toEJSON();
  if (Array.isArray(v)) return v.map((x) => (x === undefined ? null : __enc(x)));
  // Like mongosh, a field set to undefined is sent as null: dropping it
  // would turn `{ _id: user.id }` with a typo into `{}`, matching everything.
  const out = {};
  for (const k of Object.keys(v)) {
    out[k] = v[k] === undefined ? null : __enc(v[k]);
  }
  return out;
}

function __revive(_, v) {
  if (v === null || typeof v !== "object" || Array.isArray(v)) return v;
  const keys = Object.keys(v);
  if (keys.length === 0 || keys.length > 2 || keys[0][0] !== "$") return v;
  switch (keys[0]) {
    case "$oid": return new ObjectId(v.$oid);
    case "$numberInt": return Number(v.$numberInt);
    case "$numberDouble": return Number(v.$numberDouble);
    case "$numberLong": return new Long(v.$numberLong);
    case "$numberDecimal": return new Decimal128(v.$numberDecimal);
    case "$date": {
      const ms = v.$date instanceof Long ? Number(v.$date.value) : Number(v.$date);
      return new Date(ms);
    }
    case "$binary": {
      // A subtype-4 value of the wrong length stays Binary, so one bad value
      // doesn't make the whole document unreadable.
      if (v.$binary.subType === "04") {
        const hex = __base64Hex(v.$binary.base64);
        if (hex !== null && hex.length === 32) return new UUIDValue(hex);
      }
      return new Binary(v.$binary.base64, parseInt(v.$binary.subType, 16));
    }
    case "$regularExpression": {
      const { pattern, options } = v.$regularExpression;
      // JS has no x or l: keep those as BSONRegExp so writing it back keeps them.
      if (/[^imsu]/.test(options)) return new BSONRegExp(pattern, options);
      // Nor when JS would write it back differently: its `source` turns ""
      // into "(?:)", "/" into "\/" and a line break into "\n".
      try {
        const re = new RegExp(pattern, options);
        if (re.source === pattern) return re;
      } catch (_) {}
      return new BSONRegExp(pattern, options);
    }
    case "$timestamp": return new Timestamp(v.$timestamp.t, v.$timestamp.i);
    case "$minKey": return MinKey;
    case "$maxKey": return MaxKey;
    case "$undefined": return undefined;
    case "$symbol": return v.$symbol;
    case "$dbPointer": return new DBPointer(v.$dbPointer.$ref, v.$dbPointer.$id);
    case "$code": return new Code(v.$code, v.$scope);
  }
  return v;
}

function __call(op, args) {
  const reply = JSON.parse(__native(op, JSON.stringify(args)), __revive);
  if (reply.err !== undefined) {
    const e = new Error(reply.err);
    // A server error's, as mongosh has them: `if (e.code === 11000)`.
    if (reply.code !== undefined) e.code = reply.code;
    if (reply.codeName !== undefined) e.codeName = reply.codeName;
    throw e;
  }
  return reply.ok;
}

// For prelude.js, which runs next in a shell; and the entry point of a
// detached parse, which evaluates `__ejsonString([\n<text>\n])`. A second
// argument means the text closed that bracket itself.
globalThis.__bson = { enc: __enc, call: __call, revive: __revive };
globalThis.__ejsonString = (...args) => {
  if (args.length !== 1) throw new SyntaxError("the text closes a bracket it never opened");
  // A hole (`,` alone, `a,,b`) is a missing value, not null.
  for (let i = 0; i < args[0].length; i++) {
    if (!(i in args[0])) throw new SyntaxError("a value is missing between commas");
  }
  return JSON.stringify(__enc(args[0]));
};
})();
