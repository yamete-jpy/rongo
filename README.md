# rongo

A desktop MongoDB client for macOS, written in Rust (egui): a connection manager, a database tree, collection tabs with Table/Tree/JSON views, document editing, and an IntelliShell-style JavaScript shell.

## Install

With [Homebrew](https://brew.sh), on a Mac with Apple silicon:

```sh
brew tap yamete-jpy/rongo https://github.com/yamete-jpy/rongo
brew trust --cask yamete-jpy/rongo/rongo
brew install --cask rongo
```

Homebrew only loads casks from other people's taps after `brew trust`.

`brew upgrade --cask rongo` installs a newer release. You can also download the disk image from [Releases](https://github.com/yamete-jpy/rongo/releases) and drag rongo onto Applications.

The app is not notarized by Apple, so macOS blocks the first launch. Open System Settings → Privacy & Security, scroll down to "rongo" was blocked, and press Open Anyway. You only need to do this once per install.

## Build from source

```sh
cargo run --release -p rongo            # normal start
cargo run --release -p rongo -- --mock  # in-memory demo server, no MongoDB needed
./scripts/bundle-macos.sh               # builds target/rongo.app
./scripts/make-dmg.sh                   # builds target/rongo-<version>-<arch>.dmg to hand out
```

The disk image holds the app, an Applications shortcut to drag it onto, and install notes in Thai. The app is not notarized, so the first launch on another Mac needs System Settings → Privacy & Security → Open Anyway (the notes explain this). The build is for Apple silicon only.

The app icon comes from `assets/icon-1024.png`, drawn by `scripts/make-icon.py`. Re-run that script after changing the design, then run the bundle script again.

The toolchain is pinned to Rust 1.95 in `rust-toolchain.toml`; rustup installs it on first build.

## Features

- **Connections**: host/port lists or `mongodb://` / `mongodb+srv://` URIs, username/password (SCRAM), TLS (CA file, client certificate, allow-invalid), replica sets, and SSH tunnels (password or private key). Profiles are saved in `~/Library/Application Support/dev.rongo.rongo/profiles.json`. Passwords go to the macOS Keychain, never into that file. A password in a pasted URI is moved to the Keychain when you save. In the Connection Manager, tick several connections (or ⌘-/⇧-click them; the header box picks them all) and press Connect to open them all at once.
- **Order and colour**: drag a connection in the tree or in the Connection Manager to move it; the Connect menu uses the same order. Give a connection a colour (right-click → Color in the tree, the Color button on its row in the Connection Manager, or the Edit connection form) to tell production from SIT: the colour tints its row in the tree, runs along the top of its tabs, and labels the breadcrumb above the shell and results. The Connection Manager draws each row the same way as the tree.
- **Tree**: connection → replica set members → databases → collections → indexes. Each level loads when you open it. Search with ⌘F (with a match-case toggle). The right-click menus:
  - **Collection**: Open IntelliShell, Export Collection…, Import Data…, Copy Name (⌥⌘C), Rename, Duplicate (documents and indexes), Clear, Drop, Add Index…, Add/Edit Validator…, Collection Statistics, Server Info ›, Refresh, Disconnect / Disconnect Others / Disconnect All.
  - **Database**: Add Collection, Import Data, Database Statistics, Server Info ›, Drop Database (type the name to confirm).
  - **Index**: Open Index Manager, Copy Keys, Drop Index.
  - **Hover a collection** for a card with Count, Size, Storage Size, Indexes, Total Index Size and when the numbers were fetched, plus a Refresh link.
- **Add Index**: an index name (empty means MongoDB's default name, e.g. `chatID_1_sentAt_-1`) and five tabs. **Fields**: the fields in order, picked from the collection's sampled fields, each 1, -1, text, 2dsphere, 2d or hashed. **Options**: Unique, Sparse, Hidden, TTL with expiry seconds, and Partial with a filter. **Text options**: default language, language override field, weights. **Geo options**: 2dsphere version, 2d bits, min and max. **Collation**: locale, strength, case first, case level, numeric ordering. At the bottom: Create in background, and a JSON preview of exactly what will be sent. The definition goes to `createIndexes` unchanged, so every option reaches the server.
- **Index Manager** (click **Indexes (n)** under a collection, or Manage Indexes…): Name (expand to see keys and options), Type (Regular, Text, 2dsphere, Hashed, Wildcard), Properties (Compound, Unique, Sparse, TTL, Partial, Hidden), Size, and Usage from `$indexStats`. Toolbar: Refresh, Add, Drop, Edit (TTL and hidden only, which is all MongoDB can change in place), View details, Hide/Unhide. The footer shows the index count and total size.
  - **Server Info ›**: Server Status, Build Info, Host Info, Replica Set Status, Current Operations, Connection Status. Each opens in its own tab.
- **Collection tab**: Query / Projection / Sort / Skip fields in shell syntax, pages of 20 to 1000 documents, and Count Documents (an estimate when there is no filter, otherwise an exact count). Last page counts the documents first if they have not been counted yet, then jumps to the end.
- **Views**: Table view has a type icon and type colour in each cell. Double-click an object column to drill into it, or an array cell to see its elements. Tree view shows Key, Value and Type columns. JSON view shows the documents in shell syntax, with a find bar: press ⌘F with the pointer over the results (it switches to the JSON view), type, then ↩ / ⇧↩ for the next / previous match and Esc to close. Matches are highlighted, the counter shows "n of m", and **Aa** matches case. The search runs in the background, so typing stays smooth on a 50,000-document page.
- **Column headers sort**: click a header for ascending, again for descending, a third time for the query's own order. A query is re-run on the server with that sort (shown in the Sort field); a static result such as aggregate output is sorted in place.
- **Column order**: drag a column header left or right to move the column; a blue line shows where it will land, and it keeps its width. The order stays for the next pages of that result. Right-click → Column › Reset Column Order puts it back.
- **Result table right-click menu**: Field › (Edit Value, Set to null, Remove Field), Document › (Insert, Delete, View JSON, Edit JSON, Clone), Column › (Sort Ascending/Descending, Hide, Show All), Copy › (Value, Field Path, Document(s), _id), Paste Document(s), Step Into Column/Cell, Refresh View/Document, Update Dialog, Delete Dialog, Export Documents. Right-click the empty space under the rows for the selected row's menu, or, with nothing selected, Insert, Paste, Refresh View, Update/Delete Dialog and Export.
- **Paste to import**: copy documents from anywhere (rongo, Studio 3T, mongosh output, JSON, mongoexport lines, Extended JSON), click into a collection's results, and press ⌘V. rongo asks "Import N documents from the clipboard?" and can strip `_id`s so the documents insert as new ones.
- **Export / Import**: JSON lines (mongoexport style), JSON array, or CSV, for the whole collection or the current query. Import detects the format from the file. CSV numbers and booleans become typed values, and dotted headers become nested fields.
- **Update / Delete dialogs**: select documents, then right-click → Update Dialog… (⌘U) or Delete Dialog… (F8). The **Query** tab starts with `{ _id: { $in: [ … ] } }` for the selection and can be edited, or filled from *Apply predefined query* (Selected Document(s), Current Query, All Documents). The **Update** tab starts with `$set` of the cell you clicked, its type kept (e.g. `NumberLong(…)`), and supports a simple update document or an aggregation pipeline. Upsert and Multi checkboxes, and Validate JSON. An empty query with Multi on needs an extra confirmation.
- **Running work**: a query, count or document refresh that takes more than a moment shows in the result's status line (next to Count Documents) with a moving bar and the time taken; ⊗ discards it and keeps the rows on screen. Other work (connecting, loading the tree, writes, scripts in other tabs) floats in the bottom-left corner only while it runs, with its connection's name; ⊗ discards it, and **+n more** lists the rest.
- **Languages**: text in any script macOS has a font for shows as text, not boxes: Thai, Chinese, Japanese, Korean, Lao, Khmer, Myanmar, Vietnamese, and more through Arial Unicode. rongo uses macOS's own font files (memory-mapped, so a 50 MB CJK font costs only the glyphs drawn).
- **Editing**: add, edit, clone and delete documents (deleting asks for confirmation). The editor uses shell syntax and keeps BSON types: `ObjectId("…")`, `ISODate("…")`, `NumberLong("…")`, `NumberDecimal("…")`, `Double(5)`.
- **IntelliShell**: mongosh-style JavaScript on an embedded QuickJS engine. It supports `db.coll.find/aggregate/insert*/update*/replace*/delete*/countDocuments/distinct/createIndex`, cursors, `use db`, `show dbs`, `show collections`, `print`, and the BSON constructors. A `find()` the script doesn't read opens as a pageable "Find Query (line N)" tab. `aggregate()` output and a final document value open as result tabs too. Each result tab can be pinned. Stop interrupts loops and server waits. Autocomplete opens as you type: collection names and methods after `db.`; inside a query (`find({ l…`), the collection's field names (sampled from 100 documents, nested paths included) followed by every MongoDB query, update and aggregation operator; `ObjectId`, `ISODate`, … in value positions; and `"$field"` references in pipelines. Matches can be anywhere in the name, and the matching part is highlighted. ↑↓ choose, Enter or Tab accepts, Esc closes.

### Keychain prompts

All saved passwords live in **one** Keychain item, so macOS asks at most once per launch, however many connections there are. Passwords saved by older versions of rongo, one item per connection, move into it the first time each is used.

macOS ties "Always Allow" to the app's signature, and every rebuild of an unsigned app changes it. After a rebuild, macOS asks again, sometimes twice in a row: once to read the item, and once to add the new build to the item's allowed apps. To avoid that:

- Copy `target/rongo.app` to `/Applications` and replace it only when you want an update, rather than running each new build.
- Or sign builds with a certificate: Keychain Access → Certificate Assistant → Create a Certificate… → Name `rongo dev`, Identity Type *Self Signed Root*, Certificate Type *Code Signing*. Then build with `RONGO_SIGN_IDENTITY="rongo dev" ./scripts/bundle-macos.sh`.

### Settings

Open Settings with the gear button on the toolbar or ⌘,. They are saved in `~/Library/Application Support/dev.rongo.rongo/settings.json`.

- **Double-clicking a collection opens**: IntelliShell with `db.getCollection("…").find({})` already run (the default), or a collection tab with a query bar. The right-click menu offers both either way.
- **Show dates in**: UTC (the default), this computer's time zone, or a fixed offset such as UTC+07:00. The setting applies to tables, the Tree and JSON views, the document editor and shell output. Dates with an offset, such as `ISODate("2026-01-02T10:04:05.000+07:00")`, are still exact instants, so editing a document never shifts its time. Exported files always use UTC.
- **Default page size**: 1 to 50,000 (very large pages make the JSON view slow to lay out). Each result view also takes a typed size (press Enter to apply) or a preset from ▾.

### Shortcuts

| Keys | Action |
|---|---|
| ⌘↩ / F5 | Run the whole script, or only the selection |
| ⇧⌘↩ | Run the statement under the cursor (the block between blank lines) |
| Ctrl+Space | Autocomplete |
| ⌘S | Save the script, or save the document in the editor dialog |
| ⌘T / ⌘W | Open an IntelliShell tab / close the current tab |
| ⇧⌘[ / ⇧⌘] | Previous / next tab |
| ⌘F | Find in the results under the pointer (JSON view); over the sidebar, search the tree |
| ⌘, | Settings |
| ⌘A | Select all: every document (Table, Tree), or all the text (JSON, including lines off screen) |
| ⌘C / ⌘V | Copy the selection / paste documents into the collection |
| ⌘D / ⌘J / F3 | Insert a document / edit the selected document / view it as JSON |
| ⇧⌫ or Delete | Delete the selected documents |
| ⌘U / F8 | Update dialog / Delete dialog |
| ⌘R / ⇧⌘R | Refresh the view / refresh the selected document; over the sidebar, refresh every connection / the selected tree item |
| ↩ / ⇧↩ / Esc | Step into the column / step into the cell / step back out |

## Layout

```
crates/core   no UI: protocol, connections, SSH tunnel, BSON formatting, profiles, mock server
crates/shell  QuickJS runtime, mongosh-style prelude (src/prelude.js), autocomplete
crates/app    egui UI
```

The UI sends `Command`s to the core and gets `Event`s back over channels. MongoDB I/O runs on a small tokio runtime, so the UI thread never blocks on the network. Each shell tab runs scripts on its own thread.

## Tests

```sh
cargo test --workspace
```

To also run the tests against a real server:

```sh
docker run -d --rm --name rongo-test -p 127.0.0.1:27099:27017 \
  -e MONGO_INITDB_ROOT_USERNAME=root -e MONGO_INITDB_ROOT_PASSWORD=secret mongo:7
RONGO_TEST_HOST=127.0.0.1:27099 RONGO_TEST_USER=root RONGO_TEST_PASSWORD=secret cargo test --workspace
docker rm -f rongo-test
```

## Limitations

- Through an SSH tunnel, rongo connects directly to the first host. Replica set discovery would hand out member addresses that are only reachable from the far side of the tunnel.
- The SSH host key must already be in `~/.ssh/known_hosts`. Connect once with `ssh` to add it.
- TLS through an SSH tunnel checks the certificate against `127.0.0.1`, so it fails for certificates issued to the server's name (Atlas, `mongodb+srv://`, DocumentDB) unless "allow invalid certificates" is ticked.
- The tunnel's local port (127.0.0.1, random port) accepts any local process, the same as `ssh -L`. MongoDB authentication still applies to every connection.
- A find result tab is a live query: paging or refreshing it re-runs the query.
- Each shell run starts in a fresh JavaScript context, so variables do not carry over between runs. Only the current database (`use`) is kept.
- The shell ignores some options: `collation` and `arrayFilters` on updates and aggregations, `ordered: false` on `insertMany`, `createCollection` options (capped, validators), and `hint`/`maxTimeMS` on cursors. Some pasted Extended JSON forms are not read: `$timestamp`, `$minKey`/`$maxKey`, legacy `$binary` with `$type`, and mongosh 2's `Binary.createFromBase64`.
- Two rongo windows (separate app instances) saving passwords at the same time can overwrite each other's Keychain changes.
- The demo server (`--mock`) supports common query, update and aggregation operators. It reports anything else as unsupported.
- Not built yet: SQL, an aggregation editor, schema analysis, data compare, and BSON or SQL export.

rongo is an independent project and is not affiliated with 3T Software Labs or Studio 3T. The icons are [Phosphor](https://phosphoricons.com) (MIT).

## License

MIT. See [LICENSE](LICENSE).
