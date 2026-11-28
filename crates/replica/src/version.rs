//! Version gate (§8): no sync with a server this build cannot talk to;
//! local reads keep working and the user is asked to update.

use tagmail_api_core::API_VERSION;
use tagmail_api_core::dto::{ApiVersion, VersionDto};
use tagmail_core::ChangeFilter;

use crate::client::{ClientResult, ServerClient};

/// Oldest server API with the replica routes and ranged mailbox rows.
pub const MIN_SERVER: ApiVersion = ApiVersion { major: 1, minor: 2 };

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compat {
    Compatible,
    UpdateRequired {
        server: ApiVersion,
        min_client: ApiVersion,
    },
}

fn at_least(version: ApiVersion, min: ApiVersion) -> bool {
    (version.major, version.minor) >= (min.major, min.minor)
}

pub fn check(client: ApiVersion, server: &VersionDto) -> Compat {
    let compatible = client.major == server.api.major
        && at_least(client, server.min_client)
        && at_least(server.api, MIN_SERVER);
    if compatible {
        Compat::Compatible
    } else {
        Compat::UpdateRequired {
            server: server.api,
            min_client: server.min_client,
        }
    }
}

/// Whether this build and `server` both have calendars: the replica's
/// effective include set (appointments plan Decision e).
pub fn calendar(server: &VersionDto) -> bool {
    cfg!(feature = "calendar")
        && server
            .features
            .iter()
            .any(|f| f == ChangeFilter::INCLUDE_CALENDAR)
}

/// Asks the server; run before a bootstrap and on every reconnect. Also
/// says whether calendars are included.
pub async fn probe(client: &ServerClient) -> ClientResult<(Compat, bool)> {
    let server = client.version().await?;
    Ok((check(API_VERSION, &server), calendar(&server)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::canned;

    const fn v(major: u32, minor: u32) -> ApiVersion {
        ApiVersion { major, minor }
    }

    fn server(api: ApiVersion, min_client: ApiVersion) -> VersionDto {
        VersionDto {
            api,
            min_client,
            server: "test".into(),
            features: Vec::new(),
        }
    }

    #[test]
    fn version_matrix() {
        let client = v(1, 2);
        let cases = [
            (v(1, 1), v(1, 0), false),
            (v(1, 2), v(1, 0), true),
            (v(1, 5), v(1, 1), true),
            (v(2, 0), v(1, 0), false),
            (v(1, 3), v(1, 3), false),
        ];
        for (api, min_client, ok) in cases {
            let got = check(client, &server(api, min_client));
            let want = if ok {
                Compat::Compatible
            } else {
                Compat::UpdateRequired {
                    server: api,
                    min_client,
                }
            };
            assert_eq!(got, want, "server {api:?} minClient {min_client:?}");
        }
    }

    #[test]
    fn this_build_meets_its_own_minimum() {
        assert_eq!(
            check(API_VERSION, &server(API_VERSION, API_VERSION)),
            Compat::Compatible
        );
    }

    #[tokio::test]
    async fn probe_reads_the_server_version() {
        let (url, _) = canned(vec![(
            200,
            r#"{"api":{"major":1,"minor":0},"minClient":{"major":1,"minor":0},"server":"0.1"}"#,
        )])
        .await;
        let client = ServerClient::new(&url, None).unwrap();
        assert_eq!(
            probe(&client).await.unwrap(),
            (
                Compat::UpdateRequired {
                    server: v(1, 0),
                    min_client: v(1, 0),
                },
                false
            )
        );
    }

    #[test]
    fn calendars_need_the_server_feature() {
        let mut new = server(API_VERSION, v(1, 0));
        assert!(!calendar(&new));
        new.features = vec!["calendar".into()];
        assert_eq!(calendar(&new), cfg!(feature = "calendar"));
    }
}
