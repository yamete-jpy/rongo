// The mongosh-like API, evaluated once per run before the user's script.
// Rust provides one native function: __native(op, json) -> json, which
// returns {"ok": value} or {"err": message}. Values cross the boundary as
// canonical Extended JSON, so BSON types survive the round trip.
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
  toEJSON() { return { $binary: { base64: __call("hex_to_base64", { hex: this.hex }), subType: "04" } }; }
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

const MinKey = Object.freeze({ toEJSON() { return { $minKey: 1 }; } });
const MaxKey = Object.freeze({ toEJSON() { return { $maxKey: 1 }; } });

function __doubleText(n) {
  if (Number.isNaN(n)) return "NaN";
  if (n === Infinity) return "Infinity";
  if (n === -Infinity) return "-Infinity";
  return String(n);
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
      if (Number.isInteger(v) && v >= -2147483648 && v <= 2147483647 && !Object.is(v, -0)) {
        return { $numberInt: String(v) };
      }
      if (Number.isInteger(v) && Math.abs(v) <= Number.MAX_SAFE_INTEGER) {
        return { $numberLong: String(v) };
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
  if (v instanceof Cursor) throw new TypeError("a cursor cannot be stored; call .toArray() first");
  if (Array.isArray(v)) return v.map((x) => (x === undefined ? null : __enc(x)));
  // Like mongosh, a field set to undefined is sent as null: dropping it
  // would turn `{ _id: user.id }` with a typo into `{}`, matching everything.
  const out = {};
  for (const k of Object.keys(v)) {
    out[k] = v[k] === undefined ? null : __enc(v[k]);
  }
  return out;
}

// Writes need a filter, as in mongosh: a missing one is a mistake, not "all".
function __filter(filter, method) {
  if (filter === undefined || filter === null || typeof filter !== "object") {
    throw new TypeError(method + "() needs a filter document as its first argument (use {} to match every document)");
  }
  return __enc(filter);
}

function __only(o, k) {
  const keys = Object.keys(o);
  return keys.length >= 1 && keys[0] === k;
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
        const hex = __call("base64_to_hex", { base64: v.$binary.base64 });
        if (hex.length === 32) return new UUIDValue(hex);
      }
      return new Binary(v.$binary.base64, parseInt(v.$binary.subType, 16));
    }
    case "$regularExpression": {
      const { pattern, options } = v.$regularExpression;
      // JS has no x or l: keep those as BSONRegExp so writing it back keeps them.
      if (/[^imsu]/.test(options)) return new BSONRegExp(pattern, options);
      try { return new RegExp(pattern, options); }
      catch (_) { return new BSONRegExp(pattern, options); }
    }
    case "$timestamp": return new Timestamp(v.$timestamp.t, v.$timestamp.i);
    case "$minKey": return MinKey;
    case "$maxKey": return MaxKey;
    case "$undefined": return undefined;
    case "$symbol": return v.$symbol;
    case "$code": return new Code(v.$code, v.$scope);
  }
  return v;
}

function __call(op, args) {
  const reply = JSON.parse(__native(op, JSON.stringify(args)), __revive);
  if (reply.err !== undefined) throw new Error(reply.err);
  return reply.ok;
}

// ------------------------------------------------------------ output

function __callerLine() {
  const stack = new Error().stack || "";
  const m = /<shell>:(\d+)/.exec(stack);
  return m ? Number(m[1]) + globalThis.__lineOffset : 0;
}

function __text(v) {
  if (typeof v === "string") return v;
  if (v === undefined) return "undefined";
  if (v instanceof Cursor) return "Cursor on " + v._ns.db + "." + v._ns.coll;
  try { return __call("format", { value: __enc(v) }); }
  catch (_) { return String(v); }
}

globalThis.print = (...args) => { __call("print", { text: args.map(__text).join(" ") }); };
globalThis.printjson = (v) => print(v);
globalThis.console = { log: print, info: print, warn: print, error: print, debug: print };
globalThis.sleep = (ms) => { __call("sleep", { ms: Number(ms) }); };

// ------------------------------------------------------------ cursors

const __cursors = [];

class Cursor {
  constructor(ns, kind, spec) {
    this._ns = ns;
    this._kind = kind; // "find" | "aggregate"
    this._spec = spec;
    this._id = null;
    this._buf = [];
    this._done = false;
    this._touched = false;
    this._line = __callerLine();
    __cursors.push(this);
  }
  _modify(key, value) {
    if (this._id !== null) {
      // Aggregations start when created; tuning options after that is harmless.
      if (this._kind === "aggregate") return this;
      throw new Error("cursor already started; cannot change " + key);
    }
    this._spec[key] = value;
    return this;
  }
  sort(s) { return this._modify("sort", s); }
  limit(n) { return this._modify("limit", Number(n)); }
  skip(n) { return this._modify("skip", Number(n)); }
  projection(p) { return this._modify("projection", p); }
  project(p) { return this.projection(p); }
  batchSize(n) { return this._modify("batchSize", Number(n)); }
  maxTimeMS() { return this; }
  allowDiskUse() { return this; }
  hint() { return this; }
  comment() { return this; }
  pretty() { return this; }
  _fill() {
    this._touched = true;
    this._load();
  }
  _load() {
    if (this._buf.length > 0 || this._done) return;
    if (this._id === null) {
      const op = this._kind === "find" ? "find_open" : "agg_open";
      const args = { ns: this._ns };
      for (const k of Object.keys(this._spec)) args[k] = __enc(this._spec[k]);
      this._id = __call(op, args);
    }
    const r = __call("cursor_next", { cursor: this._id, n: this._spec.batchSize || 101 });
    this._buf = r.docs;
    if (r.docs.length === 0) this._done = true;
  }
  _take() {
    const d = this._buf.shift();
    return this._xf ? this._xf(d) : d;
  }
  hasNext() { this._fill(); return this._buf.length > 0; }
  tryNext() { return this.hasNext() ? this._take() : null; }
  next() {
    if (!this.hasNext()) throw new Error("no more documents in cursor");
    return this._take();
  }
  toArray() { const out = []; while (this.hasNext()) out.push(this._take()); return out; }
  forEach(f) { while (this.hasNext()) f(this._take()); }
  // Like mongosh: a cursor whose documents go through f, so .toArray(),
  // .forEach() and further .map()s work.
  map(f) {
    const prev = this._xf;
    this._xf = prev ? (d) => f(prev(d)) : f;
    return this;
  }
  itcount() { return this.toArray().length; }
  size() { return this.count(); }
  count() {
    this._touched = true;
    if (this._kind !== "find") throw new Error("count() is not available on an aggregation cursor");
    return __call("count", { ns: this._ns, filter: __enc(this._spec.filter || {}) });
  }
  close() { this._touched = true; this._done = true; }
  *[Symbol.iterator]() { while (this.hasNext()) yield this._take(); }
}

// ------------------------------------------------------------ collections

class Collection {
  constructor(db, name) { this._db = db; this._name = name; }
  get _ns() { return { db: this._db._name, coll: this._name }; }
  getName() { return this._name; }
  getFullName() { return this._db._name + "." + this._name; }
  getDB() { return this._db; }
  toString() { return this.getFullName(); }
  _w(op, args) { return __call(op, Object.assign({ ns: this._ns }, args)); }

  find(filter, projection) {
    return new Cursor(this._ns, "find", { filter: filter || {}, projection: projection });
  }
  findOne(filter, projection) {
    const r = __call("find_one", { ns: this._ns, filter: __enc(filter || {}), projection: projection && __enc(projection) });
    return r === null ? null : r;
  }
  aggregate(pipeline, _options) {
    if (!Array.isArray(pipeline)) pipeline = Array.from(arguments).filter((s) => s && typeof s === "object");
    // Run the pipeline now, in script order, so later statements (writes,
    // drops) cannot change what it returns.
    const c = new Cursor(this._ns, "aggregate", { pipeline: pipeline });
    c._load();
    return c;
  }
  countDocuments(filter) { return this._w("count", { filter: __enc(filter || {}) }); }
  count(filter) { return this.countDocuments(filter); }
  estimatedDocumentCount() { return this._w("estimated_count", {}); }
  distinct(key, filter) { return this._w("distinct", { key: key, filter: __enc(filter || {}) }); }

  insertOne(doc) {
    const id = this._w("insert_one", { doc: __enc(doc) });
    return { acknowledged: true, insertedId: id };
  }
  insertMany(docs) {
    const ids = this._w("insert_many", { docs: __enc(docs) });
    const insertedIds = {};
    ids.forEach((id, i) => { insertedIds[i] = id; });
    return { acknowledged: true, insertedIds: insertedIds };
  }
  insert(docOrDocs) { return Array.isArray(docOrDocs) ? this.insertMany(docOrDocs) : this.insertOne(docOrDocs); }
  _update(filter, update, options, many) {
    options = options || {};
    return this._w("update", { filter: __filter(filter, many ? "updateMany" : "updateOne"), update: __enc(update), many: many, upsert: !!options.upsert });
  }
  updateOne(filter, update, options) { return this._update(filter, update, options, false); }
  updateMany(filter, update, options) { return this._update(filter, update, options, true); }
  replaceOne(filter, doc, options) {
    return this._w("replace", { filter: __filter(filter, "replaceOne"), doc: __enc(doc), upsert: !!(options && options.upsert) });
  }
  deleteOne(filter) { return this._w("delete", { filter: __filter(filter, "deleteOne"), many: false }); }
  deleteMany(filter) { return this._w("delete", { filter: __filter(filter, "deleteMany"), many: true }); }
  remove(filter, justOne) { return this._w("delete", { filter: __filter(filter, "remove"), many: !justOne }); }

  createIndex(keys, options) { return this._w("create_index", { keys: __enc(keys), options: options ? __enc(options) : null }); }
  ensureIndex(keys, options) { return this.createIndex(keys, options); }
  getIndexes() { return this._w("get_indexes", {}); }
  dropIndex(name) { this._w("drop_index", { name: name }); return { ok: 1 }; }
  drop() { this._w("drop_coll", {}); return true; }
  stats() { return this._db.runCommand({ collStats: this._name }); }
}

// ------------------------------------------------------------ databases

class Database {
  constructor(name) { this._name = name; }
  getName() { return this._name; }
  toString() { return this._name; }
  getCollection(name) {
    if (typeof name !== "string" || name === "") throw new TypeError("collection name must be a non-empty string");
    return __collProxy(new Collection(this, name));
  }
  getSiblingDB(name) { return __dbProxy(new Database(name)); }
  getMongo() { return { getDB: (n) => this.getSiblingDB(n) }; }
  getCollectionNames() { return __call("list_colls", { db: this._name }); }
  getCollectionInfos() { return this.getCollectionNames().map((n) => ({ name: n })); }
  runCommand(cmd) {
    if (typeof cmd === "string") cmd = { [cmd]: 1 };
    return __call("command", { db: this._name, cmd: __enc(cmd) });
  }
  adminCommand(cmd) { return this.getSiblingDB("admin").runCommand(cmd); }
  stats() { return this.runCommand({ dbStats: 1 }); }
  version() { return this.adminCommand({ buildInfo: 1 }).version; }
  serverStatus() { return this.adminCommand({ serverStatus: 1 }); }
  hello() { return this.adminCommand({ hello: 1 }); }
  createCollection(name) { __call("create_coll", { ns: { db: this._name, coll: name } }); return { ok: 1 }; }
  dropDatabase() { __call("drop_db", { db: this._name }); return { ok: 1, dropped: this._name }; }
}

const __dbMethods = new Set(Object.getOwnPropertyNames(Database.prototype));

function __dbProxy(target) {
  return new Proxy(target, {
    get(t, prop, recv) {
      if (typeof prop === "symbol" || prop in t || __dbMethods.has(prop) || prop.startsWith("_")) {
        return Reflect.get(t, prop, recv);
      }
      return t.getCollection(prop);
    },
  });
}

// db.fs.files: a property a collection doesn't have names a sub-collection.
const __notCollections = new Set(["toJSON", "then", "inspect"]);
function __collProxy(target) {
  return new Proxy(target, {
    get(t, prop, recv) {
      if (typeof prop === "symbol" || prop in t || prop.startsWith("_") || __notCollections.has(prop)) {
        return Reflect.get(t, prop, recv);
      }
      return t._db.getCollection(t._name + "." + prop);
    },
  });
}

function __show(what) {
  switch (what) {
    case "dbs":
    case "databases":
      for (const d of __call("list_dbs", {})) print(d.name.padEnd(30) + d.size);
      return;
    case "collections":
    case "tables":
      for (const c of db.getCollectionNames()) print(c);
      return;
    default:
      throw new Error("unknown show target: " + what);
  }
}

// ------------------------------------------------------------ end of run

// Cursors the script created but never read become result tabs; the final
// value of the script is shown too, as mongosh's REPL would.
function __finish(last) {
  for (const c of __cursors) {
    if (c._touched) continue;
    if (c._kind === "find") {
      const s = c._spec;
      __call("result_find", {
        line: c._line,
        ns: c._ns,
        filter: __enc(s.filter || {}),
        sort: s.sort ? __enc(s.sort) : null,
        projection: s.projection ? __enc(s.projection) : null,
        skip: s.skip || 0,
        limit: s.limit || 0,
      });
    } else {
      const docs = [];
      let truncated = false;
      c._spec.batchSize = 500;
      while (c.hasNext()) {
        if (docs.length >= globalThis.__maxDocs) { truncated = true; break; }
        docs.push(c._buf.shift());
      }
      __call("result_docs", { line: c._line, title: "Aggregate", docs: __enc(docs), truncated: truncated });
    }
  }
  if (last === undefined || last instanceof Cursor || last instanceof Database || last instanceof Collection) {
    return db._name;
  }
  const isDoc = (x) => x !== null && typeof x === "object" && !(x instanceof Date) && !(x instanceof RegExp) &&
    typeof x.toEJSON !== "function" && !Array.isArray(x);
  if (isDoc(last) || (Array.isArray(last) && last.length > 0 && last.every(isDoc))) {
    const docs = Array.isArray(last) ? last.slice(0, globalThis.__maxDocs) : [last];
    __call("result_docs", { line: 0, title: "Result", docs: __enc(docs), truncated: Array.isArray(last) && last.length > globalThis.__maxDocs });
  } else {
    print(last);
  }
  return db._name;
}

Object.assign(globalThis, {
  __finish, __show, __dbProxy, Database,
  __ejsonString: (v) => JSON.stringify(__enc(v)),
});
})();
