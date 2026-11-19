//! What the demo server (`--mock`) serves: a small made-up shop and blog,
//! the same on every start. The tests are written against the larger test
//! data set instead (`MockStore::test_data`); `served` picks between them.

use bson::{Bson, Document, doc};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::{MockColl, MockStore, date, oid, pick};
use crate::api::Namespace;

/// The database the demo's IntelliShell tab opens on.
pub const DEMO_DB: &str = "shop";

/// The demo shell's script (shortcuts written the macOS way: the app
/// relabels them for its OS).
pub const DEMO_SCRIPT: &str = r#"// IntelliShell runs mongosh-style JavaScript. ⌘↩ runs everything (or the
// selection), ⇧⌘↩ runs the statement under the cursor. A find() you don't
// read opens as a result tab you can page through.
db.products.find({ category: "office" }).sort({ price: -1 })

db.orders.find({ status: "shipped" }).sort({ createdAt: -1 })

db.orders.aggregate([
    { $group: { _id: "$status", orders: { $sum: 1 }, items: { $sum: { $size: "$items" } } } },
    { $sort: { orders: -1 } }
])
"#;

/// Whether `served` hands out the test data: in core's own tests, and with
/// the `test-data` feature, which the app's tests turn on.
const TESTING: bool = cfg!(any(test, feature = "test-data"));

/// What the demo connection serves.
pub fn served() -> MockStore {
    if TESTING { MockStore::test_data() } else { MockStore::demo() }
}

/// The database the demo's IntelliShell tab opens on and the script in
/// it, for the data `served` hands out.
pub fn served_shell() -> (&'static str, &'static str) {
    if TESTING { (super::TEST_DB, super::TEST_SCRIPT) } else { (DEMO_DB, DEMO_SCRIPT) }
}

const FIRST: [&str; 10] = ["Alice", "Ben", "Chloe", "Daniel", "Emma", "Felix", "Grace", "Hugo", "Iris", "Jack"];
const LAST: [&str; 10] = ["Anderson", "Brooks", "Carter", "Diaz", "Evans", "Foster", "Garcia", "Hughes", "Ito", "Jensen"];
const CITIES: [(&str, &str); 10] = [
    ("Bangkok", "TH"), ("Chiang Mai", "TH"), ("Singapore", "SG"), ("Tokyo", "JP"), ("Osaka", "JP"),
    ("London", "GB"), ("Berlin", "DE"), ("Toronto", "CA"), ("Sydney", "AU"), ("Seattle", "US"),
];
/// Name, category and price in cents.
const PRODUCTS: [(&str, &str, i64); 20] = [
    ("Desk Lamp", "office", 3490), ("Notebook A5", "office", 590), ("Gel Pens (3-pack)", "office", 390), ("Monitor Stand", "office", 4900), ("Desk Organizer", "office", 1850),
    ("Coffee Mug", "kitchen", 1200), ("French Press", "kitchen", 3200), ("Chef's Knife", "kitchen", 5900), ("Cutting Board", "kitchen", 2400), ("Tea Towels", "kitchen", 1500),
    ("Throw Pillow", "home", 2200), ("Wall Clock", "home", 2900), ("Scented Candle", "home", 1650), ("Plant Pot", "home", 1400), ("Wool Blanket", "home", 4500),
    ("Water Bottle", "outdoor", 1800), ("Camping Chair", "outdoor", 5400), ("Picnic Blanket", "outdoor", 3300), ("Headlamp", "outdoor", 2700), ("Daypack", "outdoor", 6900),
];
/// Title and topic.
const POSTS: [(&str, &str); 12] = [
    ("Getting started with indexes", "databases"), ("Five ways to tidy a desk", "home"), ("A weekend in Chiang Mai", "travel"),
    ("Brewing better coffee at home", "food"), ("Why we moved to Rust", "engineering"), ("Packing light for a long trip", "travel"),
    ("Notes from a small garden", "home"), ("Reading query plans", "databases"), ("Bread for beginners", "food"),
    ("Keeping a team notebook", "work"), ("Shipping a desktop app", "engineering"), ("Rainy-day walks in Tokyo", "travel"),
];
const COMMENTS: [&str; 4] = ["Thanks, this helped.", "Great write-up!", "Could you share more detail?", "Bookmarked for later."];

/// An amount in cents, as a Decimal128 with two decimal places.
fn money(cents: i64) -> Bson {
    format!("{}.{:02}", cents / 100, cents % 100).parse::<bson::Decimal128>().map_or(Bson::Null, Bson::Decimal128)
}

fn sku(product: usize) -> String {
    format!("SKU-{}", 1001 + product)
}

impl MockStore {
    /// The demo server's data: a shop (customers, products, orders, with
    /// an index or two) and a blog, a few hundred documents in all, in
    /// every common BSON type.
    pub fn demo() -> Self {
        let mut rng = StdRng::seed_from_u64(11);
        let mut s = Self::empty();
        let mut put = |db: &str, coll: &str, docs: Vec<Document>| {
            s.dbs.entry(db.into()).or_default().insert(coll.into(), MockColl::new(docs));
        };
        put("admin", "system.version", vec![doc! { "_id": "featureCompatibilityVersion", "version": "7.0" }]);

        // Each first name meets four different last names, so every email is its own.
        let customers: Vec<Document> = (0..40)
            .map(|i| {
                let (first, last) = (FIRST[i % 10], LAST[(3 * (i % 10) + i / 10) % 10]);
                let (city, country) = CITIES[rng.random_range(0..CITIES.len())];
                let tags: Vec<&str> = ["newsletter", "wholesale", "returning", "gift"].into_iter().filter(|_| rng.random_bool(0.25)).collect();
                doc! {
                    "_id": oid(&mut rng),
                    "name": format!("{first} {last}"),
                    "email": format!("{}.{}@example.com", first.to_lowercase(), last.to_lowercase()),
                    "address": { "city": city, "country": country },
                    "tags": tags,
                    "points": rng.random_range(0..2_000_i32),
                    "active": rng.random_bool(0.85),
                    "joinedAt": date(&mut rng),
                }
            })
            .collect();
        let customer_ids: Vec<Bson> = customers.iter().filter_map(|c| c.get("_id").cloned()).collect();
        put("shop", "customers", customers);

        let products = PRODUCTS
            .iter()
            .enumerate()
            .map(|(i, &(name, category, price))| {
                let mut d = doc! {
                    "_id": oid(&mut rng),
                    "sku": sku(i),
                    "name": name,
                    "category": category,
                    "price": money(price),
                    "stock": rng.random_range(0..120_i32),
                    "rating": f64::from(rng.random_range(30..=50_i32)) / 10.0,
                };
                // Some have tags, some don't: documents needn't share fields.
                if i % 4 == 0 {
                    d.insert("tags", ["bestseller"]);
                }
                d
            })
            .collect();
        put("shop", "products", products);

        let orders = (0..200)
            .map(|i| {
                let customer = customer_ids[rng.random_range(0..customer_ids.len())].clone();
                let mut picked: Vec<usize> = Vec::new();
                let n = rng.random_range(1..=4);
                while picked.len() < n {
                    let p = rng.random_range(0..PRODUCTS.len());
                    if !picked.contains(&p) {
                        picked.push(p);
                    }
                }
                let mut total = 0;
                let items: Vec<Document> = picked
                    .iter()
                    .map(|&p| {
                        let (name, _, price) = PRODUCTS[p];
                        let qty = rng.random_range(1..=3_i32);
                        total += i64::from(qty) * price;
                        doc! { "sku": sku(p), "name": name, "qty": qty, "price": money(price) }
                    })
                    .collect();
                let created = date(&mut rng);
                let status = pick(&mut rng, &["delivered", "delivered", "shipped", "paid", "cancelled"]);
                let mut d = doc! {
                    "_id": oid(&mut rng),
                    "number": format!("ORD-{}", 10_001 + i),
                    "customerId": customer,
                    "items": items,
                    "total": money(total),
                    "status": status,
                    "createdAt": created,
                };
                if matches!(status, "shipped" | "delivered") {
                    let days = rng.random_range(1..=3_i64);
                    d.insert("shippedAt", bson::DateTime::from_millis(created.timestamp_millis() + days * 86_400_000));
                }
                d
            })
            .collect();
        put("shop", "orders", orders);

        let posts = POSTS
            .iter()
            .enumerate()
            .map(|(i, &(title, topic))| {
                let author = format!("{} {}", FIRST[i % 4], LAST[i % 4]);
                let comments: Vec<Document> = (0..rng.random_range(0..=3))
                    .map(|_| doc! { "author": pick(&mut rng, &FIRST), "text": pick(&mut rng, &COMMENTS), "at": date(&mut rng) })
                    .collect();
                let mut d = doc! {
                    "_id": oid(&mut rng),
                    "title": title,
                    "slug": title.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-"),
                    "author": author,
                    "tags": [topic],
                    "body": format!("A short note: {}.", title.to_lowercase()),
                    "views": rng.random_range(0..5_000_i32),
                    "comments": comments,
                };
                // Two drafts: no publishedAt.
                if i % 5 != 4 {
                    d.insert("publishedAt", date(&mut rng));
                }
                d
            })
            .collect();
        put("blog", "posts", posts);

        s.create_indexes(&Namespace::new("shop", "customers"), vec![doc! { "key": { "email": 1 }, "unique": true }]).expect("a new index");
        s.create_indexes(&Namespace::new("shop", "orders"), vec![doc! { "key": { "customerId": 1, "createdAt": -1 } }]).expect("a new index");
        s
    }
}

#[cfg(test)]
mod tests {
    use bson::{Bson, Document, doc};

    use super::*;
    use crate::api::Namespace;
    use crate::mock::MockStore;

    fn all(s: &MockStore, db: &str, coll: &str) -> Vec<Document> {
        s.find(&Namespace::new(db, coll), &doc! {}, None, None, 0, None).unwrap()
    }

    fn cents(v: &Bson) -> i64 {
        let Bson::Decimal128(d) = v else { panic!("a Decimal128, not {v:?}") };
        let (units, frac) = d.to_string().split_once('.').map(|(u, f)| (u.parse::<i64>().unwrap(), f.parse::<i64>().unwrap())).unwrap();
        units * 100 + frac
    }

    /// A few collections of a few hundred documents in all: enough to page
    /// through and to show every view, not a wall of data.
    #[test]
    fn the_demo_is_a_small_shop_and_a_blog() {
        let s = MockStore::demo();
        let dbs: Vec<_> = s.list_databases().into_iter().map(|d| d.name).collect();
        assert_eq!(dbs, ["admin", "blog", "shop"]);
        let colls = |db| s.list_collections(db).into_iter().map(|c| c.name).collect::<Vec<_>>();
        assert_eq!(colls("shop"), ["customers", "orders", "products"]);
        assert_eq!(colls("blog"), ["posts"]);
        let count = |db, coll| s.estimated_count(&Namespace::new(db, coll));
        assert_eq!([count("shop", "customers"), count("shop", "products"), count("shop", "orders"), count("blog", "posts")], [40, 20, 200, 12]);
        let indexes = |coll| s.list_indexes(&Namespace::new("shop", coll)).iter().map(|i| i.get_str("name").unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(indexes("customers"), ["_id_", "email_1"]);
        assert_eq!(indexes("orders"), ["_id_", "customerId_1_createdAt_-1"]);
    }

    /// Orders name real customers and products, and their totals add up.
    #[test]
    fn every_order_adds_up() {
        let s = MockStore::demo();
        let customers: Vec<Bson> = all(&s, "shop", "customers").iter().map(|c| c.get("_id").unwrap().clone()).collect();
        let products: Vec<String> = all(&s, "shop", "products").iter().map(|p| p.get_str("sku").unwrap().to_string()).collect();
        let emails: std::collections::HashSet<_> = all(&s, "shop", "customers").iter().map(|c| c.get_str("email").unwrap().to_string()).collect();
        assert_eq!(emails.len(), 40, "the unique email index holds");
        for order in all(&s, "shop", "orders") {
            assert!(customers.contains(order.get("customerId").unwrap()), "{order}");
            let items = order.get_array("items").unwrap();
            assert!((1..=4).contains(&items.len()), "{order}");
            let mut sum = 0;
            for item in items {
                let item = item.as_document().unwrap();
                assert!(products.contains(&item.get_str("sku").unwrap().to_string()), "{order}");
                sum += i64::from(item.get_i32("qty").unwrap()) * cents(item.get("price").unwrap());
            }
            assert_eq!(cents(order.get("total").unwrap()), sum, "{order}");
            let shipped = matches!(order.get_str("status").unwrap(), "shipped" | "delivered");
            assert_eq!(order.contains_key("shippedAt"), shipped, "{order}");
        }
    }

    /// Nothing in it reads like a game, a bet or a player: it is a plain shop.
    #[test]
    fn the_demo_says_nothing_of_games() {
        const WORDS: &[&str] = &["game", "games", "gaming", "bet", "bets", "betting", "player", "players", "casino", "slot", "slots", "provider", "providers", "wallet", "deposit", "withdraw", "spin", "jackpot", "odds", "agent"];
        fn words(text: &str, out: &mut Vec<String>) {
            out.extend(text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase));
        }
        fn walk(v: &Bson, out: &mut Vec<String>) {
            match v {
                Bson::String(t) => words(t, out),
                Bson::Document(d) => d.iter().for_each(|(k, v)| {
                    words(k, out);
                    walk(v, out);
                }),
                Bson::Array(a) => a.iter().for_each(|v| walk(v, out)),
                _ => {}
            }
        }
        let s = MockStore::demo();
        let mut seen = Vec::new();
        words(DEMO_SCRIPT, &mut seen);
        for db in s.list_databases() {
            words(&db.name, &mut seen);
            for coll in s.list_collections(&db.name) {
                words(&coll.name, &mut seen);
                all(&s, &db.name, &coll.name).into_iter().for_each(|d| walk(&Bson::Document(d), &mut seen));
            }
        }
        let found: Vec<_> = seen.iter().filter(|w| WORDS.contains(&w.as_str())).collect();
        assert!(found.is_empty(), "{found:?}");
    }

    /// The queries in the demo shell's script find something on the demo.
    #[test]
    fn the_demo_scripts_queries_find_something() {
        let s = MockStore::demo();
        let products = Namespace::new(DEMO_DB, "products");
        let office = s.find(&products, &doc! { "category": "office" }, Some(&doc! { "price": -1 }), None, 0, None).unwrap();
        assert!(office.len() >= 3);
        assert!(office.windows(2).all(|w| cents(w[0].get("price").unwrap()) >= cents(w[1].get("price").unwrap())));
        let orders = Namespace::new(DEMO_DB, "orders");
        assert!(!s.find(&orders, &doc! { "status": "shipped" }, Some(&doc! { "createdAt": -1 }), None, 0, None).unwrap().is_empty());
        let groups = s.aggregate(&orders, &[doc! { "$group": { "_id": "$status", "orders": { "$sum": 1 }, "items": { "$sum": { "$size": "$items" } } } }, doc! { "$sort": { "orders": -1 } }]).unwrap();
        assert_eq!(groups.len(), 4, "{groups:?}");
        assert_eq!(groups.iter().map(|g| g.get_i32("orders").unwrap()).sum::<i32>(), 200);
        assert!(DEMO_SCRIPT.contains("db.products.find({ category: \"office\" }).sort({ price: -1 })"));
        assert!(DEMO_SCRIPT.contains("db.orders.find({ status: \"shipped\" }).sort({ createdAt: -1 })"));
        assert!(DEMO_SCRIPT.contains("{ $group: { _id: \"$status\", orders: { $sum: 1 }, items: { $sum: { $size: \"$items\" } } } }"));
    }

    /// Core's own tests (and the app's, through the `test-data` feature) get
    /// the test data set, which they are written against.
    #[test]
    fn tests_are_served_the_test_data() {
        assert!(served().list_databases().iter().any(|d| d.name == crate::mock::TEST_DB));
        assert_eq!(served_shell().0, crate::mock::TEST_DB);
    }
}
