//! A demo table: a product catalogue with real words, so that the UI and
//! the agent have something to search without a dataset at hand.
//!
//! Products are made up (invented brands, template descriptions); the same
//! seed gives the same catalogue. `emb` is the embedding of the title and
//! description by the `hash` model (hashed bags of words, computed by the
//! database as rows load): products sharing words are near each other,
//! which is enough to show "more like this" and hybrid ranking, but it is
//! not a learned embedding.

use strata::schema::{Field, FieldType, Schema};
use strata::value::{Document, Value};
use strata::VectorIndexKind;

pub const DIM: usize = 64;

pub fn schema(n: usize) -> Schema {
    let index = if n > 50_000 { VectorIndexKind::Hnsw { m: 16, ef_construction: 100 } } else { VectorIndexKind::Flat };
    Schema::new(
        "products",
        vec![
            Field::new("id", FieldType::Keyword),
            Field::new("title", FieldType::Text { positions: false }).stored_only(),
            Field::new("description", FieldType::Text { positions: false }).stored_only(),
            Field::new("body", FieldType::Text { positions: true }).not_stored().generated(&["title", "description"]),
            Field::new("brand", FieldType::Keyword),
            Field::new("category", FieldType::Keyword),
            Field::new("tags", FieldType::Keyword).multi(),
            Field::new("price", FieldType::Float),
            Field::new("rating", FieldType::Float),
            Field::new("reviews", FieldType::Int),
            Field::new("sold", FieldType::Int),
            Field::new("year", FieldType::Int),
            Field::new("emb", FieldType::Vector { dim: DIM, index }).not_stored().embedded(&format!("hash:{DIM}"), &["title", "description"]),
        ],
        Some("id"),
    )
    .expect("valid schema")
}

struct Category {
    name: &'static str,
    /// Items, each with the materials it comes in.
    items: &'static [(&'static str, &'static [&'static str])],
    uses: &'static [&'static str],
    brands: &'static [&'static str],
    /// Typical price range.
    price: (f64, f64),
}

const CATEGORIES: &[Category] = &[
    Category {
        name: "outdoor",
        items: &[
            ("tent", &["ripstop nylon", "polyester", "canvas"]),
            ("sleeping bag", &["down", "synthetic fill", "recycled polyester"]),
            ("backpack", &["ripstop nylon", "waxed canvas", "recycled polyester"]),
            ("hiking boots", &["leather", "suede", "breathable mesh"]),
            ("trekking poles", &["aluminium", "carbon fibre"]),
            ("headlamp", &["aluminium", "plastic"]),
            ("rain jacket", &["breathable membrane", "ripstop nylon"]),
            ("fleece", &["recycled polyester", "merino wool"]),
            ("camp stove", &["stainless steel", "titanium"]),
            ("hammock", &["parachute nylon", "cotton"]),
        ],
        uses: &["weekend camping trips", "long day hikes", "alpine routes", "rainy trail runs", "family campsites", "thru-hikes"],
        brands: &["Northpeak", "Trailwise", "Summitry", "Ridgeline Co"],
        price: (25.0, 450.0),
    },
    Category {
        name: "kitchen",
        items: &[
            ("chef's knife", &["carbon steel", "stainless steel", "Damascus steel"]),
            ("skillet", &["cast iron", "carbon steel", "stainless steel"]),
            ("espresso machine", &["stainless steel", "brushed aluminium"]),
            ("blender", &["stainless steel", "glass"]),
            ("cutting board", &["walnut", "maple", "bamboo"]),
            ("dutch oven", &["enamelled cast iron", "cast iron"]),
            ("kettle", &["stainless steel", "glass", "copper"]),
            ("mixing bowls", &["ceramic", "glass", "stainless steel"]),
            ("stand mixer", &["die-cast metal", "stainless steel"]),
            ("pour-over set", &["ceramic", "borosilicate glass"]),
        ],
        uses: &["weeknight dinners", "slow Sunday breakfasts", "baking bread", "meal prep", "small apartments", "dinner parties"],
        brands: &["Copperleaf", "Ironvale", "Hearthly", "Saltgrain"],
        price: (12.0, 700.0),
    },
    Category {
        name: "electronics",
        items: &[
            ("wireless earbuds", &["matte plastic", "aluminium"]),
            ("noise cancelling headphones", &["aluminium", "protein leather", "fabric"]),
            ("mechanical keyboard", &["aluminium", "plastic"]),
            ("4k monitor", &["aluminium", "matte plastic"]),
            ("portable charger", &["aluminium", "plastic"]),
            ("smart speaker", &["fabric", "plastic"]),
            ("action camera", &["magnesium alloy", "plastic"]),
            ("e-reader", &["plastic", "aluminium"]),
            ("webcam", &["plastic", "aluminium"]),
            ("bluetooth speaker", &["fabric", "rubber"]),
        ],
        uses: &["remote work", "long flights", "gaming", "video calls", "commuting", "reading in bed"],
        brands: &["Voltra", "Lumio", "Sonance", "Pixelcraft"],
        price: (19.0, 900.0),
    },
    Category {
        name: "home",
        items: &[
            ("desk lamp", &["brass", "steel", "oak"]),
            ("throw blanket", &["wool", "cotton", "linen"]),
            ("air purifier", &["plastic", "steel"]),
            ("standing desk", &["oak", "bamboo", "steel"]),
            ("office chair", &["mesh", "leather", "fabric"]),
            ("bookshelf", &["oak", "walnut", "steel"]),
            ("area rug", &["wool", "jute", "cotton"]),
            ("humidifier", &["ceramic", "plastic"]),
            ("curtains", &["linen", "velvet", "cotton"]),
            ("wall clock", &["oak", "brass", "steel"]),
        ],
        uses: &["home offices", "bedrooms", "living rooms", "allergy season", "small apartments", "reading corners"],
        brands: &["Nestfield", "Oakhaven", "Driftwood & Co", "Calmhaus"],
        price: (15.0, 800.0),
    },
    Category {
        name: "sports",
        items: &[
            ("running shoes", &["breathable mesh", "knit"]),
            ("yoga mat", &["natural rubber", "cork", "TPE"]),
            ("cycling helmet", &["polycarbonate", "EPS foam"]),
            ("tennis racket", &["graphite", "carbon fibre"]),
            ("dumbbells", &["cast iron", "rubber-coated steel"]),
            ("jump rope", &["steel cable", "PVC"]),
            ("swim goggles", &["silicone", "polycarbonate"]),
            ("fitness tracker", &["silicone", "aluminium"]),
            ("foam roller", &["EVA foam", "EPP foam"]),
            ("resistance bands", &["latex", "fabric"]),
        ],
        uses: &["marathon training", "home workouts", "morning rides", "recovery days", "lap swimming", "hot yoga"],
        brands: &["Stridely", "Apexform", "Kinetiq", "Tideline"],
        price: (8.0, 350.0),
    },
];

const ADJECTIVES: &[&str] = &[
    "lightweight", "waterproof", "compact", "durable", "ergonomic", "premium", "eco-friendly", "ultralight", "insulated", "foldable",
    "rechargeable", "quiet", "adjustable", "handmade", "budget", "classic", "pro",
];

const FEATURES: &[&str] = &[
    "It folds down small enough to fit in a carry-on.",
    "Reviewers praise how quiet it is.",
    "The battery lasts well over a day of use.",
    "It comes with a lifetime warranty.",
    "Easy to clean and dishwasher safe.",
    "Made from recycled materials.",
    "It weighs less than most of its rivals.",
    "Some buyers find it runs small, so size up.",
    "Assembly takes about ten minutes without tools.",
    "It is a popular gift and ships in plain packaging.",
    "The finish scratches more easily than expected.",
    "It handles heavy daily use without wearing out.",
];

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() % xs.len() as u64) as usize]
    }
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// The `i`th product of the catalogue of `seed`.
pub fn product(i: usize, seed: u64) -> Document {
    let mut r = Rng(seed ^ (i as u64).wrapping_mul(0xD1B54A32D192ED03));
    let c = r.pick(CATEGORIES);
    let (item, materials) = *r.pick(c.items);
    let material = *r.pick(materials);
    let adj = *r.pick(ADJECTIVES);
    let adj2 = *r.pick(ADJECTIVES);
    let brand = *r.pick(c.brands);
    let title = format!("{brand} {} {material} {item}", title_case(adj));
    let lead = if item.ends_with('s') {
        title_case(adj)
    } else if adj.starts_with(['a', 'e', 'i', 'o', 'u']) {
        format!("An {adj}")
    } else {
        format!("A {adj}")
    };
    let mut description = format!("{lead} {item} in {material}, built for {}.", r.pick(c.uses));
    if adj2 != adj {
        description.push_str(&format!(" {} and easy to live with.", title_case(adj2)));
    }
    // Two different feature sentences.
    let f1 = (r.next() % FEATURES.len() as u64) as usize;
    let f2 = (f1 + 1 + (r.next() % (FEATURES.len() as u64 - 1)) as usize) % FEATURES.len();
    for f in [f1, f2] {
        description.push(' ');
        description.push_str(FEATURES[f]);
    }
    let (lo, hi) = c.price;
    // Prices skew low, like a real catalogue; premium items cost more.
    let mut price = lo + (hi - lo) * r.f().powi(3);
    if adj == "premium" {
        price *= 1.6;
    } else if adj == "budget" {
        price *= 0.6;
    }
    let price = (price * 100.0).round() / 100.0 - 0.01;
    let rating = (2.5 + 2.5 * (1.0 - r.f().powi(2))).min(5.0);
    let rating = (rating * 10.0).round() / 10.0;
    let reviews = (r.f().powi(4) * 5000.0) as i64;
    let sold = reviews * (3 + (r.next() % 20) as i64);
    let year = 2018 + (r.next() % 9) as i64;
    let mut tags = vec![adj.to_string()];
    if adj2 != adj {
        tags.push(adj2.to_string());
    }
    if rating >= 4.7 && reviews > 800 {
        tags.push("bestseller".into());
    }
    if year >= 2025 {
        tags.push("new".into());
    }
    if r.f() < 0.15 {
        tags.push("sale".into());
    }
    vec![
        Value::Str(format!("p{i:07}")),
        Value::Str(title),
        Value::Str(description),
        Value::Null,
        Value::Str(brand.into()),
        Value::Str(c.name.into()),
        Value::List(tags.into_iter().map(Value::Str).collect()),
        Value::Float(price.max(0.99)),
        Value::Float(rating),
        Value::Int(reviews),
        Value::Int(sold),
        Value::Int(year),
        Value::Null,
    ]
}
