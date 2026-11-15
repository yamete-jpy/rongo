// The mongosh-like API, evaluated once per run after bson.js and before the
// user's script.
(function () {
"use strict";

const { enc: __enc, call: __call } = globalThis.__bson;

// Writes need a filter, as in mongosh: a missing one is a mistake, not "all".
function __filter(filter, method) {
  if (filter === undefined || filter === null || typeof filter !== "object") {
    throw new TypeError(method + "() needs a filter document as its first argument (use {} to match every document)");
  }
  return __enc(filter);
}

// What the shell can't do throws, rather than being dropped without a word,
// when dropping it could change the result (`hint` among them: it can change
// which document updateOne acts on, or what a sparse index returns).
function __unsupported(what) {
  throw new TypeError("not supported: " + what);
}

// Options whose loss changes no result: accepted everywhere and ignored,
// unless a method applies them (find and aggregate apply batchSize).
const __harmless = ["comment", "maxTimeMS", "allowDiskUse", "batchSize", "writeConcern"];

// A method's options object, refusing any option not in `allowed` or
// `__harmless`.
function __opts(method, options, allowed = []) {
  if (options === undefined || options === null) return {};
  if (typeof options !== "object") throw new TypeError(method + "() options must be an object");
  for (const k of Object.keys(options)) {
    if (!allowed.includes(k) && !__harmless.includes(k)) __unsupported(method + "() option " + k);
  }
  return options;
}

// A count with skip and limit applied, as the server would.
function __skipLimit(n, skip, limit) {
  n = Math.max(0, n - Math.max(0, Number(skip) || 0));
  return limit ? Math.min(n, Math.abs(Number(limit))) : n;
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
  catch (e) {
    // Say why rather than show "[object Object]".
    let why;
    try { why = String(e); } catch (_) { why = "an error"; }
    return "<cannot print this value: " + why + ">";
  }
}

globalThis.print = (...args) => { __call("print", { text: args.map(__text).join(" ") }); };
globalThis.printjson = (v) => print(v);
globalThis.console = { log: print, info: print, warn: print, error: print, debug: print };
globalThis.sleep = (ms) => { __call("sleep", { ms: Number(ms) }); };

// ------------------------------------------------------------ cursors

// The cursors the script has not touched yet: they become result tabs at the
// end. A touched one leaves the set, so a loop of reads keeps none of them.
const __cursors = new Set();

class Cursor {
  constructor(ns, kind, spec) {
    this._ns = ns;
    this._kind = kind; // "find" | "aggregate"
    this._spec = spec;
    this._id = null;
    // The current batch and the next document in it (shift() would move
    // every remaining document each time, quadratic in a large batch).
    this._buf = [];
    this._pos = 0;
    this._done = false;
    this._touched = false;
    this._line = __callerLine();
    __cursors.add(this);
  }
  _touch() {
    if (this._touched) return;
    this._touched = true;
    __cursors.delete(this);
  }
  _modify(key, value) {
    if (this._kind === "aggregate") {
      // An aggregation runs when created: only the size of later batches
      // can still change; the rest belongs in the pipeline.
      if (key !== "batchSize") {
        const stage = { sort: "$sort", limit: "$limit", skip: "$skip", projection: "$project" }[key];
        __unsupported(key + "() on an aggregation cursor; add a " + stage + " stage to the pipeline");
      }
    } else if (this._id !== null) {
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
  // Accepted and ignored: they change no result.
  maxTimeMS() { return this; }
  allowDiskUse() { return this; }
  comment() { return this; }
  hint() { __unsupported("cursor.hint()"); }
  pretty() { return this; }
  toEJSON() { throw new TypeError("a cursor cannot be stored; call .toArray() first"); }
  _fill() {
    this._touch();
    this._load();
  }
  // An aggregation's first batch stays in Rust, as the server sent it, until
  // the script reads it: unread, it becomes a result tab as it is. With too
  // many unread (`hold`), Rust lets it go (`_dropped`): it runs again from
  // its pipeline if it is read or shown; never one that writes ($out/$merge),
  // which runs exactly once. One with no documents keeps nothing.
  _open(hold) {
    const args = { ns: this._ns };
    for (const k of Object.keys(this._spec)) args[k] = __enc(this._spec[k]);
    if (this._kind === "find") {
      this._id = __call("find_open", args);
      return;
    }
    args.hold = !!hold;
    const r = __call("agg_open", args);
    this._id = r.id;
    this._dropped = !!r.dropped;
    if (r.empty) this._done = true;
  }
  _load() {
    if (this._pos < this._buf.length || this._done) return;
    if (this._id === null) {
      this._open(false);
      if (this._done) return;
    }
    const r = __call("cursor_next", { cursor: this._id, n: this._spec.batchSize || 101 });
    this._buf = r.docs;
    this._pos = 0;
    if (r.docs.length === 0) this._done = true;
  }
  _take() {
    const d = this._buf[this._pos];
    this._buf[this._pos++] = undefined; // (the cursor doesn't keep what it handed out)
    return this._xf ? this._xf(d) : d;
  }
  hasNext() { this._fill(); return this._pos < this._buf.length; }
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
  // Like mongosh: count() ignores skip() and limit() unless asked; size() doesn't.
  size() { return this.count(true); }
  count(applySkipLimit) {
    this._touch();
    if (this._kind !== "find") throw new Error("count() is not available on an aggregation cursor");
    const n = __call("count", { ns: this._ns, filter: __enc(this._spec.filter || {}) });
    return applySkipLimit ? __skipLimit(n, this._spec.skip, this._spec.limit) : n;
  }
  close() { this._touch(); this._done = true; }
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

  find(filter, projection, options) {
    const o = __opts("find", options, ["sort", "limit", "skip", "projection"]);
    const c = new Cursor(this._ns, "find", { filter: filter || {}, projection: projection });
    for (const k of ["sort", "projection"]) if (o[k] !== undefined) c._modify(k, o[k]);
    for (const k of ["limit", "skip", "batchSize"]) if (o[k] !== undefined) c._modify(k, Number(o[k]));
    return c;
  }
  findOne(filter, projection, options) {
    __opts("findOne", options);
    const r = __call("find_one", { ns: this._ns, filter: __enc(filter || {}), projection: projection && __enc(projection) });
    return r === null ? null : r;
  }
  aggregate(pipeline, options) {
    let o = {};
    if (Array.isArray(pipeline)) o = __opts("aggregate", options, ["cursor"]);
    else pipeline = Array.from(arguments).filter((s) => s && typeof s === "object");
    const batchSize = o.batchSize || (o.cursor && o.cursor.batchSize);
    // Run the pipeline now, in script order, so later statements (writes,
    // drops) cannot change what it returns.
    const c = new Cursor(this._ns, "aggregate", batchSize ? { pipeline: pipeline, batchSize: Number(batchSize) } : { pipeline: pipeline });
    try {
      c._open(true);
    } catch (e) {
      // A failed aggregation is no unread cursor: the script saw the error.
      c._touch();
      throw e;
    }
    return c;
  }
  countDocuments(filter, options) {
    const o = __opts("countDocuments", options, ["skip", "limit"]);
    return __skipLimit(this._w("count", { filter: __enc(filter || {}) }), o.skip, o.limit);
  }
  count(filter, options) { return this.countDocuments(filter, options); }
  estimatedDocumentCount(options) { __opts("estimatedDocumentCount", options); return this._w("estimated_count", {}); }
  distinct(key, filter, options) {
    __opts("distinct", options);
    return this._w("distinct", { key: key, filter: __enc(filter || {}) });
  }

  insertOne(doc, options) {
    __opts("insertOne", options);
    const id = this._w("insert_one", { doc: __enc(doc) });
    return { acknowledged: true, insertedId: id };
  }
  insertMany(docs, options) {
    // Ordered, as it always is here; unordered would go on past a failure.
    if (__opts("insertMany", options, ["ordered"]).ordered === false) __unsupported("insertMany() option ordered: false");
    const ids = this._w("insert_many", { docs: __enc(docs) });
    const insertedIds = {};
    ids.forEach((id, i) => { insertedIds[i] = id; });
    return { acknowledged: true, insertedIds: insertedIds };
  }
  insert(docOrDocs, options) { return Array.isArray(docOrDocs) ? this.insertMany(docOrDocs, options) : this.insertOne(docOrDocs, options); }
  _update(filter, update, options, many) {
    const method = many ? "updateMany" : "updateOne";
    const o = __opts(method, options, ["upsert"]);
    return this._w("update", { filter: __filter(filter, method), update: __enc(update), many: many, upsert: !!o.upsert });
  }
  updateOne(filter, update, options) { return this._update(filter, update, options, false); }
  updateMany(filter, update, options) { return this._update(filter, update, options, true); }
  replaceOne(filter, doc, options) {
    const o = __opts("replaceOne", options, ["upsert"]);
    return this._w("replace", { filter: __filter(filter, "replaceOne"), doc: __enc(doc), upsert: !!o.upsert });
  }
  deleteOne(filter, options) { __opts("deleteOne", options); return this._w("delete", { filter: __filter(filter, "deleteOne"), many: false }); }
  deleteMany(filter, options) { __opts("deleteMany", options); return this._w("delete", { filter: __filter(filter, "deleteMany"), many: true }); }
  // remove(filter, true) or remove(filter, { justOne: true }).
  remove(filter, justOne) {
    if (justOne !== null && typeof justOne === "object") justOne = __opts("remove", justOne, ["justOne"]).justOne;
    return this._w("delete", { filter: __filter(filter, "remove"), many: !justOne });
  }

  createIndex(keys, options) { return this._w("create_index", { keys: __enc(keys), options: options ? __enc(options) : null }); }
  ensureIndex(keys, options) { return this.createIndex(keys, options); }
  getIndexes() { return this._w("get_indexes", {}); }
  dropIndex(name) { this._w("drop_index", { name: name }); return { ok: 1 }; }
  drop(options) { __opts("drop", options); this._w("drop_coll", {}); return true; }
  stats(options) {
    const o = __opts("stats", typeof options === "number" ? { scale: options } : options, ["scale"]);
    return this._db.runCommand(Object.assign({ collStats: this._name }, o));
  }
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
  // Each as { name, type } ("collection", "view" or "timeseries"); a filter
  // may match name (a string or a regex) and type.
  getCollectionInfos(filter, options) {
    __opts("getCollectionInfos", options, ["nameOnly"]);
    const f = filter === undefined || filter === null ? {} : filter;
    if (typeof f !== "object") throw new TypeError("getCollectionInfos() filter must be an object");
    for (const k of Object.keys(f)) {
      if (k !== "name" && k !== "type") __unsupported("getCollectionInfos() filter on " + k);
    }
    const matches = (want, v) => {
      if (want === undefined) return true;
      if (want instanceof RegExp) return want.test(v);
      if (typeof want === "string") return want === v;
      __unsupported("getCollectionInfos() filter other than a name or type");
    };
    return __call("coll_infos", { db: this._name }).filter((c) => matches(f.name, c.name) && matches(f.type, c.type));
  }
  runCommand(cmd) {
    if (typeof cmd === "string") cmd = { [cmd]: 1 };
    return __call("command", { db: this._name, cmd: __enc(cmd) });
  }
  adminCommand(cmd) { return this.getSiblingDB("admin").runCommand(cmd); }
  stats(options) {
    const o = __opts("stats", typeof options === "number" ? { scale: options } : options, ["scale"]);
    return this.runCommand(Object.assign({ dbStats: 1 }, o));
  }
  version() { return this.adminCommand({ buildInfo: 1 }).version; }
  serverStatus() { return this.adminCommand({ serverStatus: 1 }); }
  hello() { return this.adminCommand({ hello: 1 }); }
  createCollection(name, options) {
    __opts("createCollection", options);
    __call("create_coll", { ns: { db: this._name, coll: name } });
    return { ok: 1 };
  }
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

// Rejected promises nobody handled (Rust's rejection tracker calls this);
// the first is the run's error once its promise jobs are done.
const __unhandled = new Map();
function __rejection(promise, reason, handled) {
  if (handled) __unhandled.delete(promise);
  else __unhandled.set(promise, reason);
}

// At most this many unread cursors become result tabs in a run.
const __maxTabs = 20;

const __isDoc = (x) => x !== null && typeof x === "object" && !(x instanceof Date) && !(x instanceof RegExp) &&
  !(x instanceof Promise) && typeof x.toEJSON !== "function" && !Array.isArray(x);

// The final value, or what a mapped cursor gives: a document or an array of
// documents becomes a result tab, anything else is printed.
function __present(v, line, truncated) {
  if (__isDoc(v) || (Array.isArray(v) && v.length > 0 && v.every(__isDoc))) {
    const docs = Array.isArray(v) ? v.slice(0, globalThis.__maxDocs) : [v];
    truncated = truncated || (Array.isArray(v) && v.length > globalThis.__maxDocs);
    __call("result_docs", { line: line, title: "Result", docs: __enc(docs), truncated: truncated });
  } else {
    print(v);
    if (truncated) print("(the first " + globalThis.__maxDocs + " shown)");
  }
}

// Cursors the script created but never read become result tabs; the final
// value of the script is shown too, as mongosh's REPL would.
function __finish(last) {
  for (const reason of __unhandled.values()) throw reason;
  const unread = Array.from(__cursors);
  for (const c of unread.slice(0, __maxTabs)) {
    if (c._xf) {
      // Its documents go through the cursor's map(): show what that gives.
      const vals = [];
      while (vals.length <= globalThis.__maxDocs && c.hasNext()) vals.push(c._take());
      const truncated = vals.length > globalThis.__maxDocs;
      if (truncated) vals.pop();
      __present(vals, c._line, truncated);
    } else if (c._kind === "find") {
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
      // The documents go from the server to the tab without passing through
      // JS, so they keep their exact types (a whole-number Double stays one).
      // One let go with too many unread runs again from its pipeline.
      if (c._dropped) c._open(false);
      if (c._done) __call("result_docs", { line: c._line, title: "Aggregate", docs: [], truncated: false });
      else __call("result_cursor", { line: c._line, title: "Aggregate", cursor: c._id });
    }
  }
  if (unread.length > __maxTabs) {
    const more = unread.length - __maxTabs;
    print(more + " more unread cursor" + (more === 1 ? "" : "s") + " not shown: a run opens at most " + __maxTabs + " result tabs");
  }
  if (last === undefined || last instanceof Cursor || last instanceof Database || last instanceof Collection) {
    return db._name;
  }
  // (A settled promise arrives as its value.)
  if (last instanceof Promise) {
    print("Promise { <pending> }");
    return db._name;
  }
  __present(last, 0, false);
  return db._name;
}

Object.assign(globalThis, { __finish, __show, __dbProxy, __rejection, Database });
})();
