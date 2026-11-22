//! The licences a listing can carry (docs/plan.md › Licences), in plain words. The wording is a
//! draft: the owner decides the final text (legal wording is the owner's, docs/start-here.md).

/// What one licence lets people do.
#[derive(Debug, PartialEq)]
pub struct Licence {
    /// Kept in the database.
    pub id: &'static str,
    pub name: &'static str,
    /// What it allows, in a sentence or two.
    pub summary: &'static str,
    /// The licence's own official text (every licence that isn't MakerFoundry's own).
    pub url: Option<&'static str>,
    /// Which kinds of design may carry it.
    pub scope: Scope,
}

/// Which kinds of design a licence is offered for (MF-35): the software and hardware licences
/// makers use for printable models (docs/3d-hub-plan.md § 6) make no sense on a stitch file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Embroidery, laser and 3D designs alike.
    Both,
    /// 3D models only.
    Model3d,
}

/// How many items a small business may sell from a design (draft: the owner decides).
pub const SMALL_BUSINESS_ITEMS: u32 = 200;

/// For the designs MakerFoundry's own licences cover: the files themselves are never shared.
const NO_SHARING: &str = "Don't share, sell or give away the design file itself.";

pub const LICENCES: &[Licence] = &[
    Licence {
        id: "personal",
        name: "Personal use",
        summary: "Make things from this design for yourself and as gifts. Don't sell what you make.",
        url: None,
        scope: Scope::Both,
    },
    Licence {
        id: "small-business",
        name: "Small business",
        summary: "Make things from this design and sell up to 200 of them.",
        url: None,
        scope: Scope::Both,
    },
    Licence {
        id: "commercial",
        name: "Commercial",
        summary: "Make things from this design and sell as many as you like.",
        url: None,
        scope: Scope::Both,
    },
    Licence {
        id: "cc-by",
        name: "Creative Commons Attribution (CC BY 4.0)",
        summary: "Anyone may use, change and share this design, for any purpose, as long as they credit its maker.",
        url: Some("https://creativecommons.org/licenses/by/4.0/"),
        scope: Scope::Both,
    },
    Licence {
        id: "cc-by-nc",
        name: "Creative Commons Attribution-NonCommercial (CC BY-NC 4.0)",
        summary: "Anyone may use, change and share this design, crediting its maker, but not to make money.",
        url: Some("https://creativecommons.org/licenses/by-nc/4.0/"),
        scope: Scope::Both,
    },
    Licence {
        id: "cc0",
        name: "Public domain (CC0 1.0)",
        summary: "No rights kept: anyone may use this design for anything.",
        url: Some("https://creativecommons.org/publicdomain/zero/1.0/"),
        scope: Scope::Both,
    },
    Licence {
        id: "cc-by-sa",
        name: "Creative Commons Attribution-ShareAlike (CC BY-SA 4.0)",
        summary: "Anyone may use, change and share this model, for any purpose, crediting its maker; changed versions must be shared under the same licence.",
        url: Some("https://creativecommons.org/licenses/by-sa/4.0/"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "cc-by-nc-sa",
        name: "Creative Commons Attribution-NonCommercial-ShareAlike (CC BY-NC-SA 4.0)",
        summary: "Anyone may use, change and share this model, crediting its maker, but not to make money; changed versions must be shared under the same licence.",
        url: Some("https://creativecommons.org/licenses/by-nc-sa/4.0/"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "cc-by-nd",
        name: "Creative Commons Attribution-NoDerivatives (CC BY-ND 4.0)",
        summary: "Anyone may share this model as it is, for any purpose, crediting its maker, but not share changed versions.",
        url: Some("https://creativecommons.org/licenses/by-nd/4.0/"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "cc-by-nc-nd",
        name: "Creative Commons Attribution-NonCommercial-NoDerivatives (CC BY-NC-ND 4.0)",
        summary: "Anyone may share this model as it is, crediting its maker, but not to make money and not changed.",
        url: Some("https://creativecommons.org/licenses/by-nc-nd/4.0/"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "gpl-2.0-or-later",
        name: "GNU General Public License v2 or later (GPL-2.0-or-later)",
        summary: "Anyone may use, change and share this model; whoever shares it or a changed version must pass on its source files under the same licence.",
        url: Some("https://www.gnu.org/licenses/old-licenses/gpl-2.0.html"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "gpl-3.0-or-later",
        name: "GNU General Public License v3 or later (GPL-3.0-or-later)",
        summary: "Anyone may use, change and share this model; whoever shares it or a changed version must pass on its source files under the same licence.",
        url: Some("https://www.gnu.org/licenses/gpl-3.0.html"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "lgpl-2.1-or-later",
        name: "GNU Lesser General Public License v2.1 or later (LGPL-2.1-or-later)",
        summary: "Anyone may use, change and share this model, and use it in larger works under other terms; changes to the model itself stay under this licence.",
        url: Some("https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "lgpl-3.0-or-later",
        name: "GNU Lesser General Public License v3 or later (LGPL-3.0-or-later)",
        summary: "Anyone may use, change and share this model, and use it in larger works under other terms; changes to the model itself stay under this licence.",
        url: Some("https://www.gnu.org/licenses/lgpl-3.0.html"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "bsd-3-clause",
        name: "BSD 3-Clause",
        summary: "Anyone may use, change and share this model, for any purpose, keeping its copyright notice and not using the maker's name to promote their own work.",
        url: Some("https://opensource.org/license/bsd-3-clause"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "apache-2.0",
        name: "Apache License 2.0",
        summary: "Anyone may use, change and share this model, for any purpose, keeping its notices and saying what they changed.",
        url: Some("https://www.apache.org/licenses/LICENSE-2.0"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "mit",
        name: "MIT",
        summary: "Anyone may use, change and share this model, for any purpose, keeping its copyright notice.",
        url: Some("https://opensource.org/license/mit"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "cern-ohl-s",
        name: "CERN Open Hardware Licence v2, strongly reciprocal (CERN-OHL-S-2.0)",
        summary: "Anyone may make, change and share this design; whoever shares it or something made from it must share all its source files under the same licence.",
        url: Some("https://ohwr.org/cern_ohl_s_v2.txt"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "cern-ohl-w",
        name: "CERN Open Hardware Licence v2, weakly reciprocal (CERN-OHL-W-2.0)",
        summary: "Anyone may make, change and share this design; changes to the design itself must be shared under the same licence, other parts needn't be.",
        url: Some("https://ohwr.org/cern_ohl_w_v2.txt"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "cern-ohl-p",
        name: "CERN Open Hardware Licence v2, permissive (CERN-OHL-P-2.0)",
        summary: "Anyone may make, change and share this design, for any purpose, keeping its notices.",
        url: Some("https://ohwr.org/cern_ohl_p_v2.txt"),
        scope: Scope::Model3d,
    },
    Licence {
        id: "all-rights-reserved",
        name: "All rights reserved",
        summary: "The maker keeps all rights: print it for yourself, but don't sell prints or share changed versions without the maker's permission.",
        url: None,
        scope: Scope::Model3d,
    },
];

pub fn find(id: &str) -> Option<&'static Licence> {
    LICENCES.iter().find(|l| l.id == id)
}

/// The licences a design of this kind (`embroidery`, `laser`, `model3d`) may carry.
pub fn for_kind(kind: &str) -> Vec<&'static Licence> {
    LICENCES.iter().filter(|l| l.allowed_for(kind)).collect()
}

/// A licence by id, if a design of this kind may carry it.
pub fn find_for(kind: &str, id: &str) -> Option<&'static Licence> {
    find(id).filter(|l| l.allowed_for(kind))
}

/// The message shown when a NonCommercial licence is paired with a price (#133).
pub const NC_NO_SALE: &str = "This licence doesn't allow selling. Pick another licence or make it free.";

/// True for the Creative Commons NonCommercial licences: a listing under one can't have a price.
pub fn forbids_selling(id: &str) -> bool {
    matches!(id, "cc-by-nc" | "cc-by-nc-sa" | "cc-by-nc-nd")
}

impl Licence {
    fn allowed_for(&self, kind: &str) -> bool {
        self.scope == Scope::Both || kind == "model3d"
    }

    /// The full plain-words text shown on a design's page.
    pub fn plain(&self) -> String {
        match self.url {
            Some(_) => self.summary.to_string(),
            None => format!("{} {NO_SHARING}", self.summary),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_noncommercial_licences_forbid_selling() {
        for id in ["cc-by-nc", "cc-by-nc-sa", "cc-by-nc-nd"] {
            assert!(forbids_selling(id), "{id}");
            assert!(find(id).is_some(), "{id}");
        }
        for l in LICENCES.iter().filter(|l| !l.id.contains("-nc")) {
            assert!(!forbids_selling(l.id), "{}", l.id);
        }
    }

    #[test]
    fn a_3d_licence_cant_go_on_an_embroidery_design() {
        assert!(find_for("embroidery", "gpl-3.0-or-later").is_none());
        assert!(find_for("laser", "cc-by-sa").is_none());
        assert!(find_for("model3d", "gpl-3.0-or-later").is_some());
        assert!(find_for("model3d", "all-rights-reserved").is_some());
        // the ones both kinds use
        for id in ["personal", "small-business", "commercial", "cc-by", "cc-by-nc", "cc0"] {
            assert!(find_for("embroidery", id).is_some(), "{id}");
            assert!(find_for("model3d", id).is_some(), "{id}");
        }
        assert!(find_for("model3d", "nope").is_none());
        assert!(for_kind("embroidery").iter().all(|l| l.scope != Scope::Model3d));
    }

    #[test]
    fn every_3d_licence_has_a_summary_and_an_official_link() {
        let three_d = for_kind("model3d");
        // Creative Commons (6 + CC0), GNU (4), BSD, Apache, MIT, CERN (3), ours (3), all rights reserved
        assert_eq!(three_d.len(), 21);
        for l in &three_d {
            assert!(l.summary.len() > 20, "{}", l.id);
            let ours = ["personal", "small-business", "commercial", "all-rights-reserved"].contains(&l.id);
            match l.url {
                Some(u) => assert!(u.starts_with("https://") && !ours, "{}", l.id),
                None => assert!(ours, "{} needs its official text", l.id),
            }
        }
        let mut ids: Vec<_> = LICENCES.iter().map(|l| l.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), LICENCES.len());
    }

    #[test]
    fn licences_are_found_by_id_and_mf_licences_keep_files_private() {
        assert_eq!(find("commercial").map(|l| l.name), Some("Commercial"));
        assert!(find("nope").is_none());
        assert!(find("personal").unwrap().plain().contains("Don't share"));
        assert!(!find("cc0").unwrap().plain().contains("Don't share"));
        assert!(
            find("small-business")
                .unwrap()
                .summary
                .contains(&SMALL_BUSINESS_ITEMS.to_string())
        );
        let mut ids: Vec<_> = LICENCES.iter().map(|l| l.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), LICENCES.len());
    }
}
