use eclipse_webview::proto::{CookieExpiry, SameSite, StoredCookie};
use gtk4::glib;
use webkit6::soup;

const SESSION_COOKIE_MAX_AGE: i32 = -1;

pub(crate) fn to_soup(cookie: &StoredCookie) -> Result<soup::Cookie, glib::BoolError> {
    if cookie.domain.is_empty() {
        return Err(glib::bool_error!("cookie {:?} has no domain", cookie.name));
    }
    let mut soup_cookie = soup::Cookie::new(
        &cookie.name,
        &cookie.value,
        &cookie.domain,
        &cookie.path,
        SESSION_COOKIE_MAX_AGE,
    );
    soup_cookie.set_secure(cookie.secure);
    soup_cookie.set_http_only(cookie.http_only);
    soup_cookie.set_same_site_policy(match cookie.same_site {
        SameSite::None => soup::SameSitePolicy::None,
        SameSite::Lax => soup::SameSitePolicy::Lax,
        SameSite::Strict => soup::SameSitePolicy::Strict,
    });
    if let CookieExpiry::At { epoch_s } = cookie.expiry {
        soup_cookie.set_expires(&glib::DateTime::from_unix_utc(epoch_s)?);
    }
    Ok(soup_cookie)
}

pub(crate) fn from_soup(cookie: &mut soup::Cookie) -> StoredCookie {
    StoredCookie {
        name: cookie.name().map(String::from).unwrap_or_default(),
        value: cookie.value().map(String::from).unwrap_or_default(),
        domain: cookie.domain().map(String::from).unwrap_or_default(),
        path: cookie.path().map(String::from).unwrap_or_default(),
        secure: cookie.is_secure(),
        http_only: cookie.is_http_only(),
        same_site: match cookie.same_site_policy() {
            soup::SameSitePolicy::Strict => SameSite::Strict,
            soup::SameSitePolicy::Lax => SameSite::Lax,
            _ => SameSite::None,
        },
        expiry: match cookie.expires() {
            Some(expires) => CookieExpiry::At {
                epoch_s: expires.to_unix(),
            },
            None => CookieExpiry::Session,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eclipse_webview::cookie_jar;
    use std::time::SystemTime;

    fn cookie(name: &str, expiry: CookieExpiry) -> StoredCookie {
        StoredCookie {
            name: name.to_string(),
            value: "v".to_string(),
            domain: ".roblox.com".to_string(),
            path: "/".to_string(),
            secure: true,
            http_only: true,
            same_site: SameSite::Lax,
            expiry,
        }
    }

    #[test]
    fn cookies_keep_every_field_through_libsoup() {
        for original in [
            cookie("session", CookieExpiry::Session),
            cookie(
                "persistent",
                CookieExpiry::At {
                    epoch_s: 1_900_000_000,
                },
            ),
            StoredCookie {
                domain: "www.roblox.com".to_string(),
                secure: false,
                http_only: false,
                same_site: SameSite::Strict,
                ..cookie("host-only", CookieExpiry::Session)
            },
        ] {
            let mut soup_cookie = to_soup(&original).expect("soup cookie");
            assert_eq!(from_soup(&mut soup_cookie), original);
        }
    }

    #[test]
    fn a_cookie_without_a_domain_is_rejected_before_libsoup_sees_it() {
        let mut domainless = cookie("orphan", CookieExpiry::Session);
        domainless.domain.clear();
        assert!(to_soup(&domainless).is_err());
    }

    fn within_a_second(ours: &StoredCookie, theirs: &StoredCookie) -> bool {
        let expiry_close = match (ours.expiry, theirs.expiry) {
            (CookieExpiry::At { epoch_s: a }, CookieExpiry::At { epoch_s: b }) => {
                a.abs_diff(b) <= 1
            }
            (a, b) => a == b,
        };
        expiry_close
            && StoredCookie {
                expiry: theirs.expiry,
                ..ours.clone()
            } == *theirs
    }

    #[test]
    fn the_host_jar_reads_set_cookie_headers_as_libsoup_does() {
        let url = "https://www.roblox.com/games/1818/details";
        let origin = glib::Uri::parse(url, glib::UriFlags::NONE).expect("parse the URL");
        for header in [
            ".ROBLOSECURITY=_|WARNING:-DO-NOT-SHARE-THIS.|_token; domain=.roblox.com; HttpOnly; \
             secure; expires=Fri, 02 Oct 2054 21:00:00 GMT; path=/",
            "RBXEventTrackerV2=CreateDate=10/2/2026 9:53:04 PM&rbxid=&browserid=1790949822023019; \
             expires=Tue, 17 Feb 2054 21:53:04 GMT; path=/; domain=.roblox.com",
            "RBXSessionTracker=sessionid=1; domain=roblox.com; path=/",
            "GuestData=UserID=-1; domain=.roblox.com; expires=Sat, 31 Jan 2054 21:53:04 GMT; path=/",
            "host-only=1",
            "own-host=1; Domain=www.roblox.com",
            "brief=1; Max-Age=3600; Path=/games",
            "relative-path=1; Path=relative",
            "missing-same-site=1; Secure",
            "same-site-none=1; SameSite=None; Secure",
            "same-site-strict=1; SameSite=Strict",
            "same-site-lax=1; samesite=lax",
            ".ROBLOSECURITY=;domain=.roblox.com;path=/;expires=Wed, 10 May 2000 00:00:00 GMT",
        ] {
            let ours = cookie_jar::parse_set_cookie(url, header, SystemTime::now())
                .unwrap_or_else(|error| panic!("the jar refused {header:?}: {error}"));
            let mut parsed = soup::Cookie::parse(header, Some(&origin))
                .unwrap_or_else(|| panic!("libsoup refused {header:?}"));
            let theirs = from_soup(&mut parsed);
            assert!(
                within_a_second(&ours, &theirs),
                "{header:?}\n jar: {ours:?}\n libsoup: {theirs:?}"
            );
        }
        let mismatched = "elsewhere=1; Domain=example.com";
        assert!(cookie_jar::parse_set_cookie(url, mismatched, SystemTime::now()).is_err());
        assert!(soup::Cookie::parse(mismatched, Some(&origin)).is_none());
    }
}
