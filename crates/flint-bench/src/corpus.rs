//! Seeded synthetic corpus: a Windows-shaped tree of `entries` entries.
//!
//! Shape (by share of entries): C: 70% (user files, AppData caches,
//! node_modules forests, Program Files, WinSxS-like noise), D: 20%
//! (backups, projects), E: 10% (music, photos). Names mix English and
//! Turkish words (with ç ş ğ ı İ ö ü), years, versions and realistic
//! extensions; ~2% carry an emoji and ~1% are 200+ chars long.

use flint_core::{EntryId, EntryInput, Index};
use rand::seq::IndexedRandom;
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

const EN: &[&str] = &[
    "report",
    "budget",
    "invoice",
    "summary",
    "meeting",
    "notes",
    "draft",
    "final",
    "backup",
    "photo",
    "image",
    "video",
    "music",
    "project",
    "plan",
    "design",
    "review",
    "contract",
    "offer",
    "quote",
    "receipt",
    "statement",
    "tax",
    "salary",
    "payroll",
    "schedule",
    "agenda",
    "minutes",
    "proposal",
    "presentation",
    "slides",
    "chart",
    "data",
    "export",
    "import",
    "config",
    "settings",
    "readme",
    "license",
    "changelog",
    "index",
    "main",
    "app",
    "server",
    "client",
    "test",
    "spec",
    "build",
    "release",
    "debug",
    "log",
    "cache",
    "temp",
    "old",
    "new",
    "copy",
    "archive",
    "letter",
    "resume",
    "cover",
    "portfolio",
    "travel",
    "holiday",
    "family",
    "birthday",
    "wedding",
    "party",
    "school",
    "homework",
    "lecture",
    "course",
    "exam",
    "thesis",
    "paper",
    "article",
    "book",
    "chapter",
    "recipe",
    "shopping",
    "list",
    "todo",
    "ideas",
    "journal",
    "diary",
    "health",
    "insurance",
    "bank",
    "loan",
    "rent",
    "lease",
    "car",
    "house",
    "garden",
    "kitchen",
    "office",
    "team",
    "client",
    "customer",
    "sales",
    "marketing",
    "finance",
    "legal",
    "hr",
    "support",
    "ticket",
    "issue",
    "bug",
    "feature",
    "roadmap",
    "strategy",
    "quarter",
    "annual",
    "monthly",
    "weekly",
    "daily",
    "sprint",
    "backlog",
    "release",
    "version",
    "patch",
    "update",
    "install",
    "setup",
    "driver",
    "firmware",
    "manual",
    "guide",
    "tutorial",
    "sample",
    "template",
    "form",
    "survey",
    "results",
    "analysis",
    "forecast",
    "metrics",
    "dashboard",
    "kpi",
    "audit",
    "compliance",
    "policy",
    "security",
    "password",
    "account",
    "profile",
    "avatar",
    "logo",
    "banner",
    "icon",
    "font",
    "theme",
    "style",
    "layout",
    "mockup",
    "wireframe",
    "prototype",
    "sketch",
    "render",
    "model",
    "texture",
    "scene",
    "level",
    "map",
    "save",
    "screenshot",
    "recording",
    "podcast",
    "episode",
    "interview",
    "transcript",
    "subtitle",
    "playlist",
    "album",
    "track",
    "song",
    "cover",
    "lyrics",
    "concert",
    "ticket",
    "flight",
    "hotel",
    "booking",
    "itinerary",
    "passport",
    "visa",
    "id",
    "license",
    "certificate",
    "diploma",
    "award",
    "contest",
    "game",
    "score",
    "stats",
    "node",
    "package",
    "module",
    "library",
    "utils",
    "helpers",
    "types",
    "core",
    "common",
    "shared",
    "vendor",
    "dist",
    "src",
    "lib",
    "bin",
    "assets",
    "public",
    "static",
    "images",
    "fonts",
    "styles",
    "scripts",
    "docs",
    "examples",
    "fixtures",
    "mocks",
    "snapshots",
    "coverage",
    "reports",
    "logs",
    "tmp",
    "data",
    "db",
    "schema",
    "migration",
    "seed",
    "query",
    "api",
    "routes",
    "handlers",
    "middleware",
    "auth",
    "users",
    "orders",
    "products",
    "cart",
    "checkout",
    "payment",
    "shipping",
    "inventory",
    "warehouse",
    "supplier",
    "vendor",
    "partner",
    "contract",
    "agreement",
    "nda",
    "terms",
    "privacy",
    "cookie",
    "consent",
    "gdpr",
    "kvkk",
    "draft",
    "v2",
    "copy2",
    "edited",
    "fixed",
    "working",
    "scratch",
    "untitled",
    "new folder",
];

const TR: &[&str] = &[
    "bütçe",
    "rapor",
    "ışık",
    "finans",
    "sunum",
    "fatura",
    "özet",
    "toplantı",
    "sözleşme",
    "teklif",
    "dekont",
    "makbuz",
    "vergi",
    "maaş",
    "bordro",
    "takvim",
    "gündem",
    "tutanak",
    "öneri",
    "tablo",
    "veri",
    "yedek",
    "arşiv",
    "eski",
    "yeni",
    "kopya",
    "taslak",
    "son",
    "nihai",
    "mektup",
    "özgeçmiş",
    "dilekçe",
    "başvuru",
    "fotoğraf",
    "resim",
    "video",
    "müzik",
    "şarkı",
    "albüm",
    "tatil",
    "aile",
    "doğum",
    "günü",
    "düğün",
    "okul",
    "ödev",
    "ders",
    "sınav",
    "tez",
    "makale",
    "kitap",
    "bölüm",
    "tarif",
    "alışveriş",
    "liste",
    "yapılacaklar",
    "fikirler",
    "günlük",
    "sağlık",
    "sigorta",
    "banka",
    "kredi",
    "kira",
    "araba",
    "ev",
    "bahçe",
    "mutfak",
    "ofis",
    "ekip",
    "müşteri",
    "satış",
    "pazarlama",
    "muhasebe",
    "hukuk",
    "insan",
    "kaynakları",
    "destek",
    "talep",
    "sorun",
    "hata",
    "özellik",
    "yol",
    "haritası",
    "strateji",
    "çeyrek",
    "yıllık",
    "aylık",
    "haftalık",
    "proje",
    "plan",
    "tasarım",
    "inceleme",
    "kurulum",
    "ayarlar",
    "sürüm",
    "güncelleme",
    "sürücü",
    "kılavuz",
    "rehber",
    "örnek",
    "şablon",
    "form",
    "anket",
    "sonuçlar",
    "analiz",
    "tahmin",
    "denetim",
    "politika",
    "güvenlik",
    "şifre",
    "hesap",
    "profil",
    "logo",
    "afiş",
    "simge",
    "yazı",
    "tipi",
    "tema",
    "düzen",
    "çizim",
    "model",
    "doku",
    "sahne",
    "harita",
    "kayıt",
    "ekran",
    "görüntüsü",
    "röportaj",
    "çeviri",
    "altyazı",
    "konser",
    "bilet",
    "uçuş",
    "otel",
    "rezervasyon",
    "pasaport",
    "vize",
    "kimlik",
    "ehliyet",
    "sertifika",
    "diploma",
    "ödül",
    "yarışma",
    "oyun",
    "puan",
    "istatistik",
    "çalışma",
    "belge",
    "dosya",
    "klasör",
    "not",
    "notlar",
    "hatırlatma",
    "randevu",
    "doktor",
    "reçete",
    "tahlil",
    "fiş",
    "gider",
    "gelir",
    "kâr",
    "zarar",
    "bilanço",
    "nakit",
    "akış",
    "yatırım",
    "hisse",
    "fon",
    "döviz",
    "altın",
    "emlak",
    "tapu",
    "aidat",
    "elektrik",
    "su",
    "doğalgaz",
    "internet",
    "telefon",
    "abonelik",
    "üyelik",
    "kurs",
    "eğitim",
    "seminer",
    "konferans",
    "çalıştay",
    "staj",
    "iş",
    "ilanı",
    "mülakat",
    "teklifname",
    "şartname",
    "ihale",
    "hakediş",
    "keşif",
    "metraj",
    "proje",
    "çizimleri",
    "statik",
    "mimari",
    "elektrik",
    "mekanik",
    "peyzaj",
    "iç",
    "mimarlık",
    "ölçüm",
    "deney",
    "laboratuvar",
    "numune",
    "kalite",
    "kontrol",
    "üretim",
    "sevkiyat",
    "depo",
    "stok",
    "tedarik",
    "sipariş",
    "iade",
    "garanti",
    "servis",
    "bakım",
    "arıza",
    "onarım",
    "yedek",
    "parça",
    "katalog",
    "fiyat",
    "listesi",
    "indirim",
    "kampanya",
    "bülten",
    "duyuru",
    "basın",
    "bildirisi",
    "sosyal",
    "medya",
    "gönderi",
    "paylaşım",
    "İstanbul",
    "İzmir",
    "Ankara",
    "Çanakkale",
    "Muğla",
    "Şanlıurfa",
    "Iğdır",
    "Ağrı",
];

const EMOJI: &[&str] = &["🎵", "📁", "📷", "🎉", "✅", "🔥", "💼", "📝", "🧾", "🚀"];
const SEPS: &[&str] = &["_", "-", " ", ""];

#[derive(Clone, Copy)]
enum Area {
    User,
    AppData,
    NodeModules,
    ProgramFiles,
    WinSxS,
    Backup,
    Media,
}

impl Area {
    fn exts(self) -> &'static [&'static str] {
        match self {
            Area::User => &[
                "docx", "xlsx", "pdf", "pptx", "jpg", "png", "txt", "mp4", "zip", "ods", "odt",
                "csv", "md", "heic",
            ],
            Area::AppData => &[
                "tmp", "dat", "json", "log", "cache", "db", "bin", "ldb", "js",
            ],
            Area::NodeModules => &["js", "json", "ts", "md", "map", "cjs", "mjs", "d.ts"],
            Area::ProgramFiles => &["exe", "dll", "ini", "xml", "pak", "dat", "json", "png"],
            Area::WinSxS => &["dll", "manifest", "mui", "cat", "sys", "exe"],
            Area::Backup => &[
                "zip", "7z", "bak", "docx", "xlsx", "pdf", "jpg", "sql", "tar",
            ],
            Area::Media => &[
                "mp3", "flac", "jpg", "jpeg", "png", "mp4", "mov", "heic", "m4a",
            ],
        }
    }
    /// Share of new entries in this area that are folders.
    fn dir_share(self) -> f64 {
        match self {
            Area::NodeModules | Area::WinSxS => 0.18,
            _ => 0.12,
        }
    }
}

struct Pool {
    area: Area,
    weight: u32,
    dirs: Vec<EntryId>,
}

struct Gen {
    rng: ChaCha8Rng,
    ix: Index,
    /// Every name ever produced, so the same seed gives the same index.
    buf: String,
}

impl Gen {
    fn dir(&mut self, name: &str, parent: Option<EntryId>) -> EntryId {
        let last_change = self.time();
        self.ix.insert(EntryInput {
            name,
            parent,
            is_dir: true,
            last_change,
        })
    }

    fn time(&mut self) -> u32 {
        // 2019-01-01 .. 2026-10-01, skewed toward recent
        let r: f64 = self.rng.random_range(0.0..1.0);
        1_546_300_800 + ((1.0 - r * r) * 244_000_000.0) as u32
    }

    fn word(&mut self) -> &'static str {
        if self.rng.random_bool(0.5) {
            EN.choose(&mut self.rng).unwrap()
        } else {
            TR.choose(&mut self.rng).unwrap()
        }
    }

    fn stem(&mut self) -> String {
        let n = self.rng.random_range(1..=3);
        let sep = *SEPS.choose(&mut self.rng).unwrap();
        let mut s = String::new();
        for i in 0..n {
            let w = self.word();
            if i > 0 {
                s.push_str(sep);
            }
            if sep.is_empty() && i > 0 {
                let mut cs = w.chars();
                if let Some(c) = cs.next() {
                    s.extend(c.to_uppercase());
                    s.push_str(cs.as_str());
                }
            } else {
                s.push_str(w);
            }
        }
        match self.rng.random_range(0..10) {
            0..=1 => s.push_str(&format!("_{}", self.rng.random_range(2015..=2026))),
            2 => s.push_str(&format!("_v{}", self.rng.random_range(1..10))),
            3 => s.push_str(&format!(" ({})", self.rng.random_range(1..5))),
            _ => {}
        }
        if self.rng.random_bool(0.02) {
            s = format!("{} {s}", EMOJI.choose(&mut self.rng).unwrap());
        }
        if self.rng.random_bool(0.01) {
            while s.chars().count() < 200 {
                s.push('_');
                let w = self.word();
                s.push_str(w);
            }
        }
        s
    }

    fn fill(&mut self, pools: &mut [Pool], total: usize) {
        let weight_sum: u32 = pools.iter().map(|p| p.weight).sum();
        while self.ix.live_len() < total {
            let mut pick = self.rng.random_range(0..weight_sum);
            let pi = pools
                .iter()
                .position(|p| {
                    if pick < p.weight {
                        true
                    } else {
                        pick -= p.weight;
                        false
                    }
                })
                .unwrap();
            let area = pools[pi].area;
            let parent = *pools[pi].dirs.choose(&mut self.rng).unwrap();
            let is_dir = self.rng.random_bool(area.dir_share());
            self.buf = self.stem();
            if !is_dir {
                let ext = *area.exts().choose(&mut self.rng).unwrap();
                self.buf.push('.');
                self.buf.push_str(ext);
            }
            let t = self.time();
            let id = self.ix.insert(EntryInput {
                name: &self.buf,
                parent: Some(parent),
                is_dir,
                last_change: t,
            });
            if is_dir {
                pools[pi].dirs.push(id);
            }
        }
    }
}

/// Deterministic for a given `(seed, entries)`. `entries` must be at least
/// the size of the fixed skeleton (~40).
pub fn generate(seed: u64, entries: usize) -> Index {
    let mut g = Gen {
        rng: ChaCha8Rng::seed_from_u64(seed),
        ix: Index::new(),
        buf: String::new(),
    };

    let c = g.dir("C:", None);
    let users = g.dir("Users", Some(c));
    let deniz = g.dir("Deniz", Some(users));
    let user_dirs: Vec<EntryId> = ["Desktop", "Documents", "Downloads", "Pictures", "Music"]
        .iter()
        .map(|n| g.dir(n, Some(deniz)))
        .collect();
    let docs = user_dirs[1];
    let fin = g.dir("Finans", Some(docs));
    let appdata = g.dir("AppData", Some(deniz));
    let local = g.dir("Local", Some(appdata));
    let roaming = g.dir("Roaming", Some(appdata));
    let temp = g.dir("Temp", Some(local));
    let code = g.dir("Code", Some(deniz));
    let nm: Vec<EntryId> = ["webapp", "api", "tools"]
        .iter()
        .map(|p| {
            let proj = g.dir(p, Some(code));
            g.dir("node_modules", Some(proj))
        })
        .collect();
    let pf = g.dir("Program Files", Some(c));
    let pf_apps: Vec<EntryId> = [
        "Common Files",
        "Microsoft Office",
        "Mozilla Firefox",
        "7-Zip",
        "Git",
    ]
    .iter()
    .map(|n| g.dir(n, Some(pf)))
    .collect();
    let windows = g.dir("Windows", Some(c));
    let winsxs = g.dir("WinSxS", Some(windows));
    let sys32 = g.dir("System32", Some(windows));
    let d = g.dir("D:", None);
    let d_dirs: Vec<EntryId> = ["Yedek", "Projeler", "Arşiv"]
        .iter()
        .map(|n| g.dir(n, Some(d)))
        .collect();
    let e = g.dir("E:", None);
    let e_dirs: Vec<EntryId> = ["Müzik", "Fotoğraflar", "Videolar"]
        .iter()
        .map(|n| g.dir(n, Some(e)))
        .collect();

    let mut user = user_dirs.clone();
    user.push(fin);
    let mut pools = vec![
        Pool {
            area: Area::User,
            weight: 15,
            dirs: user,
        },
        Pool {
            area: Area::AppData,
            weight: 20,
            dirs: vec![local, roaming, temp],
        },
        Pool {
            area: Area::NodeModules,
            weight: 15,
            dirs: nm,
        },
        Pool {
            area: Area::ProgramFiles,
            weight: 10,
            dirs: pf_apps,
        },
        Pool {
            area: Area::WinSxS,
            weight: 10,
            dirs: vec![winsxs, sys32],
        },
        Pool {
            area: Area::Backup,
            weight: 20,
            dirs: d_dirs,
        },
        Pool {
            area: Area::Media,
            weight: 10,
            dirs: e_dirs,
        },
    ];
    assert!(entries >= g.ix.live_len(), "entries below skeleton size");
    g.fill(&mut pools, entries);
    g.ix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ix: &Index) -> Vec<String> {
        (0..ix.raw_len() as u32)
            .map(|i| ix.path(EntryId(i)))
            .collect()
    }
    fn has_name_containing(ix: &Index, s: &str) -> bool {
        (0..ix.raw_len() as u32).any(|i| ix.name(EntryId(i)).contains(s))
    }
    fn has_ancestor_named(ix: &Index, s: &str) -> bool {
        (0..ix.raw_len() as u32)
            .any(|i| !ix.is_dir(EntryId(i)) && ix.ancestors(EntryId(i)).any(|a| ix.name(a) == s))
    }

    #[test]
    fn deterministic() {
        assert_eq!(names(&generate(42, 10_000)), names(&generate(42, 10_000)));
        assert_ne!(names(&generate(42, 10_000)), names(&generate(43, 10_000)));
    }

    #[test]
    fn size_and_shape() {
        let ix = generate(7, 50_000);
        assert_eq!(ix.live_len(), 50_000);
        assert!(has_name_containing(&ix, "ç") && has_name_containing(&ix, "ı"));
        assert!(has_ancestor_named(&ix, "node_modules"));
        assert!((0..ix.raw_len() as u32).any(|i| ix.name(EntryId(i)).chars().count() >= 200));
        assert!(
            (0..ix.raw_len() as u32)
                .any(|i| ix.name(EntryId(i)).chars().any(|c| c as u32 > 0xFFFF))
        );
    }
}
