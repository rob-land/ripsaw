// TheDiscDB role -> per-scheme extras folder name. See docs/naming.md.

use crate::identify::TitleRole;
use crate::settings::SchemeKind;

pub fn folder_for_role(scheme: SchemeKind, role: TitleRole) -> &'static str {
    match (scheme, role) {
        (SchemeKind::Jellyfin | SchemeKind::Emby, r) => match r {
            TitleRole::Trailer         => "trailers",
            TitleRole::BehindTheScenes => "behindthescenes",
            TitleRole::DeletedScene    => "deleted",
            TitleRole::Featurette      => "featurettes",
            TitleRole::Interview       => "interviews",
            TitleRole::Scene           => "scenes",
            TitleRole::Short           => "shorts",
            TitleRole::Other           => "extras",
            TitleRole::Main            => "",
        },
        (SchemeKind::Plex, r) => match r {
            TitleRole::Trailer         => "Trailers",
            TitleRole::BehindTheScenes => "Behind The Scenes",
            TitleRole::DeletedScene    => "Deleted Scenes",
            TitleRole::Featurette      => "Featurettes",
            TitleRole::Interview       => "Interviews",
            TitleRole::Scene           => "Scenes",
            TitleRole::Short           => "Shorts",
            TitleRole::Other           => "Other",
            TitleRole::Main            => "",
        },
        (SchemeKind::Kodi, r) => match r {
            TitleRole::Trailer => "",   // inline with -trailer suffix
            TitleRole::Main    => "",
            _                  => "extras",
        },
    }
}
