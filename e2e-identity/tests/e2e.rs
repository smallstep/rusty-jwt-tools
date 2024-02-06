#![cfg(not(target_family = "wasm"))]

use jwt_simple::prelude::*;
use serde_json::{json, Value};
use testcontainers::clients::Cli;

use rusty_acme::prelude::*;
use rusty_jwt_tools::prelude::*;
use utils::{
    cfg::{E2eTest, EnrollmentFlow, OidcProvider},
    docker::{stepca::CaCfg, wiremock::WiremockImage},
    id_token::resign_id_token,
    rand_base64_str, rand_client_id,
    wire_server::OauthCfg,
    TestError,
};

#[path = "utils/mod.rs"]
mod utils;

fn docker() -> &'static Cli {
    Box::leak(Box::new(Cli::new::<testcontainers::core::env::Os>()))
}

/// Tests the nominal case and prints the pretty output with the mermaid chart in this crate README.
#[cfg(not(ci))]
#[tokio::test]
async fn demo_should_succeed() {
    let test = E2eTest::new_demo().start(docker()).await;
    test.nominal_enrollment().await.unwrap();
}

#[cfg(not(ci))]
#[tokio::test]
#[ignore] // since we cannot customize the id token
async fn demo_with_dex_should_succeed() {
    let demo = E2eTest::new_internal(true, JwsAlgorithm::Ed25519, OidcProvider::Dex);
    let test = demo.start(docker()).await;
    test.nominal_enrollment().await.unwrap();
}

/// Tests the nominal case and prints the pretty output with the mermaid chart in this crate README.
#[ignore] // interactive test. Uncomment to try it.
#[cfg(not(ci))]
#[tokio::test]
async fn google_demo_should_succeed() {
    let default = E2eTest::new_demo();
    let issuer = "https://accounts.google.com".to_string();
    let client_secret = std::env::var("GOOGLE_E2EI_DEMO_CLIENT_SECRET")
        .expect("You have to set the client secret in the 'GOOGLE_E2EI_DEMO_CLIENT_SECRET' env variable");
    let audience = "338888153072-ktbh66pv3mr0ua0dn64sphgimeo0p7ss.apps.googleusercontent.com".to_string();
    let jwks_uri = "https://www.googleapis.com/oauth2/v3/certs".to_string();
    let domain = "wire.com";
    let new_sub =
        ClientId::try_from_raw_parts(default.sub.user_id.as_ref(), default.sub.device_id, domain.as_bytes()).unwrap();
    let test = E2eTest {
        domain: domain.to_string(),
        sub: new_sub,
        display_name: "Beltram Maldant".to_string(),
        handle: "beltram_wire".to_string(),
        oauth_cfg: OauthCfg {
            client_secret,
            client_id: audience.clone(),
            ..default.oauth_cfg
        },
        ca_cfg: CaCfg {
            issuer,
            audience,
            jwks_uri,
            ..default.ca_cfg
        },
        oidc_provider: OidcProvider::Google,
        ..default
    };
    let test = test.start(docker()).await;
    assert!(test.nominal_enrollment().await.is_ok());
}

/// Verify that it works for all MLS ciphersuites
#[cfg(not(ci))]
mod alg {
    use super::*;

    #[tokio::test]
    async fn ed25519_should_succeed() {
        let test = E2eTest::new_internal(false, JwsAlgorithm::Ed25519, OidcProvider::Dex)
            .start(docker())
            .await;
        assert!(test.nominal_enrollment().await.is_ok());
    }

    #[tokio::test]
    async fn p256_should_succeed() {
        let test = E2eTest::new_internal(false, JwsAlgorithm::P256, OidcProvider::Dex)
            .start(docker())
            .await;
        assert!(test.nominal_enrollment().await.is_ok());
    }

    // TODO: Fails because of hardcoded SHA-256 hash algorithm in stepca
    #[ignore]
    #[tokio::test]
    async fn p384_should_succeed() {
        let test = E2eTest::new_internal(false, JwsAlgorithm::P384, OidcProvider::Dex)
            .start(docker())
            .await;
        assert!(test.nominal_enrollment().await.is_ok());
    }
}

/// Since the acme server is a fork, verify its invariants are respected
#[cfg(not(ci))]
mod acme_server {
    use rusty_acme::prelude::RustyAcmeError;

    use super::*;

    /// Challenges returned by ACME server are mixed up
    #[tokio::test]
    async fn should_fail_when_no_replay_nonce_requested() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            get_acme_nonce: Box::new(|test, _| {
                Box::pin(async move {
                    // this replay nonce has not been generated by the acme server
                    let unknown_replay_nonce = rand_base64_str(42);
                    Ok((test, unknown_replay_nonce))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::AccountCreationError
        ));
    }

    /// Replay nonce is reused by the client
    #[tokio::test]
    async fn should_fail_when_replay_nonce_reused() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            new_order: Box::new(|mut test, (directory, account, previous_nonce)| {
                Box::pin(async move {
                    // same nonce is used for both 'new_order' & 'new_authz'
                    let (order, order_url, _previous_nonce) =
                        test.new_order(&directory, &account, previous_nonce.clone()).await?;
                    let (_, _, previous_nonce) =
                        test.new_authorization(&account, order.clone(), previous_nonce).await?;
                    Ok((test, (order, order_url, previous_nonce)))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::AuthzCreationError
        ));
    }

    /// Challenges returned by ACME server are mixed up
    #[tokio::test]
    async fn should_fail_when_challenges_inverted() {
        let test = E2eTest::new().start(docker()).await;

        let real_chall = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (real_chall_setter, rc1, rc2) = (real_chall.clone(), real_chall.clone(), real_chall.clone());

        let flow = EnrollmentFlow {
            extract_challenges: Box::new(|mut test, (authz_a, authz_b)| {
                Box::pin(async move {
                    let (dpop_chall, oidc_chall) = test.extract_challenges(authz_b, authz_a)?;
                    *real_chall_setter.lock().unwrap() = Some(dpop_chall.clone());
                    // let's invert those challenges for the rest of the flow
                    Ok((test, (oidc_chall, dpop_chall)))
                })
            }),
            // undo the inversion here to verify that it fails on acme server side (we do not want to test wire-server here)
            create_dpop_token: Box::new(|mut test, (_, nonce, handle, team, expiry)| {
                Box::pin(async move {
                    let challenge = rc1.lock().unwrap().clone().unwrap();
                    let dpop_token = test.create_dpop_token(&challenge, nonce, handle, team, expiry).await?;
                    Ok((test, dpop_token))
                })
            }),
            get_access_token: Box::new(|mut test, (_, dpop_token)| {
                Box::pin(async move {
                    let challenge = rc2.lock().unwrap().clone().unwrap();
                    let access_token = test.get_access_token(&challenge, dpop_token).await?;
                    Ok((test, access_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }

    /// Since this call a custom method on our acme server fork, verify we satisfy the invariant:
    /// request payloads must be signed by the same client key which created the acme account.
    ///
    /// This verifies the DPoP challenge verification method on the acme server
    #[tokio::test]
    async fn should_fail_when_dpop_challenge_signed_by_a_different_key() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            verify_dpop_challenge: Box::new(|mut test, (account, dpop_chall, access_token, previous_nonce)| {
                Box::pin(async move {
                    let old_kp = test.acme_kp;
                    // use another key just for signing this request
                    test.acme_kp = Ed25519KeyPair::generate().to_pem().into();
                    let previous_nonce = test
                        .verify_dpop_challenge(&account, dpop_chall, access_token, previous_nonce)
                        .await?;
                    test.acme_kp = old_kp;
                    Ok((test, previous_nonce))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::DpopChallengeError
        ));
    }

    /// Since this call a custom method on our acme server fork, verify we satisfy the invariant:
    /// request payloads must be signed by the same client key which created the acme account.
    ///
    /// This verifies the DPoP challenge verification method on the acme server
    #[tokio::test]
    async fn should_fail_when_oidc_challenge_signed_by_a_different_key() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            verify_oidc_challenge: Box::new(|mut test, (account, oidc_chall, access_token, previous_nonce)| {
                Box::pin(async move {
                    let old_kp = test.acme_kp;
                    // use another key just for signing this request
                    test.acme_kp = Ed25519KeyPair::generate().to_pem().into();
                    let previous_nonce = test
                        .verify_oidc_challenge(&account, oidc_chall, access_token, previous_nonce)
                        .await?;
                    test.acme_kp = old_kp;
                    Ok((test, previous_nonce))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::OidcChallengeError
        ));
    }
}

#[cfg(not(ci))]
mod dpop_challenge {
    use super::*;

    /// Demonstrates that the client possesses the clientId. Client makes an authenticated request
    /// to wire-server, it delivers a nonce which the client seals in a signed DPoP JWT.
    #[tokio::test]
    async fn should_fail_when_client_dpop_token_has_wrong_backend_nonce() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            create_dpop_token: Box::new(|mut test, (dpop_chall, backend_nonce, handle, team, expiry)| {
                Box::pin(async move {
                    // use a different nonce than the supplied one
                    let wrong_nonce = rand_base64_str(32).into();
                    assert_ne!(wrong_nonce, backend_nonce);

                    let client_dpop_token = test
                        .create_dpop_token(&dpop_chall, wrong_nonce, handle, team, expiry)
                        .await?;
                    Ok((test, client_dpop_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::WireServerError
        ));
    }

    /// Acme server should be configured with wire-server public key to verify the access tokens
    /// issued by wire-server.
    #[tokio::test]
    async fn should_fail_when_access_token_not_signed_by_wire_server() {
        let default = E2eTest::new();
        let wrong_backend_kp = Ed25519KeyPair::generate();
        let test = E2eTest {
            ca_cfg: CaCfg {
                sign_key: wrong_backend_kp.public_key().to_pem(),
                ..default.ca_cfg
            },
            ..default
        };
        let test = test.start(docker()).await;
        assert!(matches!(
            test.nominal_enrollment().await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }

    /// The access token has a 'chal' claim which should match the Acme challenge 'token'.
    /// This is verified by the acme server
    #[tokio::test]
    async fn should_fail_when_access_token_challenge_claim_is_not_current_challenge_one() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            create_dpop_token: Box::new(|mut test, (dpop_chall, backend_nonce, handle, team, expiry)| {
                Box::pin(async move {
                    // alter the 'token' of the valid challenge
                    let wrong_dpop_chall = AcmeChallenge {
                        token: rand_base64_str(32),
                        ..dpop_chall
                    };
                    let client_dpop_token = test
                        .create_dpop_token(&wrong_dpop_chall, backend_nonce, handle, team, expiry)
                        .await?;
                    Ok((test, client_dpop_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }

    /// We first set a clientId for the enrollment process when we create the acme order. This same
    /// clientId must be used and sealed in the accessToken which is verified by the acme server in
    /// the oidc challenge. The challenge should be invalid if they differ
    #[tokio::test]
    async fn should_fail_when_access_token_client_id_mismatches() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            new_order: Box::new(|mut test, (directory, account, previous_nonce)| {
                Box::pin(async move {
                    // just alter the clientId for the order creation...
                    let sub = test.sub.clone();
                    test.sub = rand_client_id(Some(sub.device_id));
                    let (order, order_url, previous_nonce) =
                        test.new_order(&directory, &account, previous_nonce).await?;
                    // ...then resume to the regular one to create the client dpop token & access token
                    test.sub = sub;
                    Ok((test, (order, order_url, previous_nonce)))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }

    /// Client DPoP token is nested within access token. The former should not be expired when
    /// acme server verifies the DPoP challenge
    // TODO: not testable in practice because leeway of 360s is hardcoded in acme server
    #[ignore]
    #[should_panic]
    #[tokio::test]
    async fn should_fail_when_expired_client_dpop_token() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            create_dpop_token: Box::new(|mut test, (dpop_chall, backend_nonce, handle, team, _expiry)| {
                Box::pin(async move {
                    let leeway = 360;
                    let expiry = core::time::Duration::from_secs(0);
                    let client_dpop_token = test
                        .create_dpop_token(&dpop_chall, backend_nonce, handle, team, expiry)
                        .await?;
                    tokio::time::sleep(core::time::Duration::from_secs(leeway + 1)).await;
                    Ok((test, client_dpop_token))
                })
            }),
            ..Default::default()
        };
        test.enrollment(flow).await.unwrap();
    }

    /// In order to tie DPoP challenge verification on the acme server, the latter is configured
    /// with the accepted wire-server host which is present in the DPoP "htu" claim and in the access token
    /// "iss" claim.
    /// The challenge should fail if any of those does not match the expected value
    #[tokio::test]
    async fn should_fail_when_access_token_iss_mismatches_target() {
        // "iss" in access token mismatches expected target
        let test = E2eTest::new().start(docker()).await;

        let nonce_arc = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (nonce_w, nonce_r) = (nonce_arc.clone(), nonce_arc.clone());

        let flow = EnrollmentFlow {
            create_dpop_token: Box::new(|mut test, (dpop_chall, nonce, handle, team, expiry)| {
                Box::pin(async move {
                    *nonce_w.lock().unwrap() = Some(nonce.clone());
                    let client_dpop_token = test.create_dpop_token(&dpop_chall, nonce, handle, team, expiry).await?;
                    Ok((test, client_dpop_token))
                })
            }),
            get_access_token: Box::new(|test, (dpop_chall, _)| {
                Box::pin(async move {
                    let client_id = test.sub.clone();
                    let htu: Htu = "https://unknown.io".try_into().unwrap();
                    let backend_nonce: BackendNonce = nonce_r.lock().unwrap().clone().unwrap();
                    let acme_nonce: AcmeNonce = dpop_chall.token.as_str().into();
                    let handle = Handle::from(test.handle.as_str())
                        .try_to_qualified(&client_id.domain)
                        .unwrap();
                    let audience = dpop_chall.url.clone();

                    let client_dpop_token = RustyJwtTools::generate_dpop_token(
                        Dpop {
                            htm: Htm::Post,
                            htu: htu.clone(),
                            challenge: acme_nonce,
                            handle: handle.clone(),
                            team: test.team.clone().into(),
                            extra_claims: None,
                        },
                        &client_id,
                        backend_nonce.clone(),
                        audience,
                        core::time::Duration::from_secs(3600),
                        test.alg,
                        &test.acme_kp,
                    )
                    .unwrap();

                    let backend_kp: Pem = test.backend_kp.clone();
                    let access_token = RustyJwtTools::generate_access_token(
                        &client_dpop_token,
                        &client_id,
                        handle,
                        test.team.clone().into(),
                        backend_nonce,
                        htu,
                        Htm::Post,
                        360,
                        2136351646,
                        backend_kp,
                        test.hash_alg,
                        5,
                        core::time::Duration::from_secs(360),
                    )
                    .unwrap();
                    Ok((test, access_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }

    /// see [should_fail_when_access_token_iss_mismatches_target]
    #[tokio::test]
    async fn should_fail_when_access_token_device_id_mismatches_target() {
        // "iss" deviceId mismatches the actual deviceId
        let test = E2eTest::new().start(docker()).await;

        let nonce_arc = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (nonce_w, nonce_r) = (nonce_arc.clone(), nonce_arc.clone());

        let flow = EnrollmentFlow {
            create_dpop_token: Box::new(|mut test, (dpop_chall, nonce, handle, team, expiry)| {
                Box::pin(async move {
                    *nonce_w.lock().unwrap() = Some(nonce.clone());
                    let client_dpop_token = test.create_dpop_token(&dpop_chall, nonce, handle, team, expiry).await?;
                    Ok((test, client_dpop_token))
                })
            }),
            get_access_token: Box::new(|test, (dpop_chall, _)| {
                Box::pin(async move {
                    // here the DeviceId will be different in "sub" than in "iss" (in the access token)
                    let client_id = ClientId {
                        device_id: 42,
                        ..test.sub.clone()
                    };
                    let htu: Htu = dpop_chall.target.into();
                    let backend_nonce: BackendNonce = nonce_r.lock().unwrap().clone().unwrap();
                    let acme_nonce: AcmeNonce = dpop_chall.token.as_str().into();
                    let handle = Handle::from(test.handle.as_str())
                        .try_to_qualified(&client_id.domain)
                        .unwrap();
                    let audience = dpop_chall.url.clone();

                    let client_dpop_token = RustyJwtTools::generate_dpop_token(
                        Dpop {
                            htm: Htm::Post,
                            htu: htu.clone(),
                            challenge: acme_nonce,
                            handle: handle.clone(),
                            team: test.team.clone().into(),
                            extra_claims: None,
                        },
                        &client_id,
                        backend_nonce.clone(),
                        audience,
                        core::time::Duration::from_secs(3600),
                        test.alg,
                        &test.acme_kp,
                    )
                    .unwrap();

                    let backend_kp: Pem = test.backend_kp.clone();
                    let access_token = RustyJwtTools::generate_access_token(
                        &client_dpop_token,
                        &client_id,
                        handle,
                        test.team.clone().into(),
                        backend_nonce,
                        htu,
                        Htm::Post,
                        360,
                        2136351646,
                        backend_kp,
                        test.hash_alg,
                        5,
                        core::time::Duration::from_secs(360),
                    )
                    .unwrap();
                    Ok((test, access_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }

    /// Demonstrates that the client possesses the handle. This handle is included in the DPoP token,
    /// then verified and sealed in the access token which is finally verified by the ACME server
    /// as part of the DPoP challenge.
    /// Here we make the acme-server fail.
    #[tokio::test]
    async fn acme_should_fail_when_client_dpop_token_has_wrong_handle() {
        let test = E2eTest::new().start(docker()).await;

        let flow = EnrollmentFlow {
            create_dpop_token: Box::new(|mut test, (dpop_chall, backend_nonce, _handle, team, expiry)| {
                Box::pin(async move {
                    let wrong_handle = Handle::from("other_wire").try_to_qualified("wire.com").unwrap();
                    let client_dpop_token = test
                        .create_dpop_token(&dpop_chall, backend_nonce, wrong_handle, team, expiry)
                        .await?;
                    Ok((test, client_dpop_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::WireServerError
        ));
    }

    /// The access token (forged by wire-server) contains a 'kid' claim which is the JWK thumbprint of the public part
    /// of the keypair used in the ACME account. This constrains the ACME client to be the issuer of the DPoP token.
    ///
    /// In this attack, a malicious server forges an access token with a forged proof (the client DPoP token). Since it
    /// does not know the keypair used by the client it will use a random one. This should fail since the acme-server
    /// will verify the 'cnf.kid' and verify that it is indeed the JWK thumbprint of the ACME client.
    #[tokio::test]
    async fn acme_should_fail_when_client_dpop_token_has_wrong_kid() {
        let test = E2eTest::new().start(docker()).await;

        let nonce_arc = std::sync::Arc::new(std::sync::Mutex::new(None));
        let (nonce_w, nonce_r) = (nonce_arc.clone(), nonce_arc.clone());

        let flow = EnrollmentFlow {
            create_dpop_token: Box::new(|mut test, (dpop_chall, nonce, handle, team, expiry)| {
                Box::pin(async move {
                    *nonce_w.lock().unwrap() = Some(nonce.clone());
                    let client_dpop_token = test.create_dpop_token(&dpop_chall, nonce, handle, team, expiry).await?;
                    Ok((test, client_dpop_token))
                })
            }),
            get_access_token: Box::new(|test, (dpop_chall, _)| {
                Box::pin(async move {
                    let client_id = test.sub.clone();
                    let htu: Htu = dpop_chall.target.into();
                    let backend_nonce: BackendNonce = nonce_r.lock().unwrap().clone().unwrap();
                    let handle = Handle::from(test.handle.as_str())
                        .try_to_qualified(&client_id.domain)
                        .unwrap();
                    let acme_nonce: AcmeNonce = dpop_chall.token.as_str().into();
                    let audience = dpop_chall.url.clone();

                    // use the MLS keypair instead of the ACME one, should make the validation fail on the acme-server
                    let keypair = test.client_kp.clone();
                    let client_dpop_token = RustyJwtTools::generate_dpop_token(
                        Dpop {
                            htm: Htm::Post,
                            htu: htu.clone(),
                            challenge: acme_nonce,
                            handle: handle.clone(),
                            team: test.team.clone().into(),
                            extra_claims: None,
                        },
                        &test.sub,
                        backend_nonce.clone(),
                        audience,
                        core::time::Duration::from_secs(3600),
                        test.alg,
                        &keypair,
                    )
                    .unwrap();

                    let backend_kp: Pem = test.backend_kp.clone();
                    let access_token = RustyJwtTools::generate_access_token(
                        &client_dpop_token,
                        &client_id,
                        handle,
                        test.team.clone().into(),
                        backend_nonce,
                        htu,
                        Htm::Post,
                        360,
                        2136351646,
                        backend_kp,
                        test.hash_alg,
                        5,
                        core::time::Duration::from_secs(360),
                    )
                    .unwrap();
                    Ok((test, access_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }

    /// We bind the DPoP challenge "uri" to the access token. It is then validated by the ACME server
    #[tokio::test]
    async fn should_fail_when_invalid_dpop_audience() {
        let test = E2eTest::new().start(docker()).await;
        let flow = EnrollmentFlow {
            create_dpop_token: Box::new(|mut test, (mut dpop_chall, backend_nonce, handle, team, expiry)| {
                Box::pin(async move {
                    // change the url in the DPoP challenge to alter what's in the DPoP token, then restore it at the end
                    let dpop_challenge_url = dpop_chall.url.clone();
                    dpop_chall.url = "http://unknown.com".parse().unwrap();

                    let client_dpop_token = test
                        .create_dpop_token(&dpop_chall, backend_nonce, handle, team, expiry)
                        .await?;

                    dpop_chall.url = dpop_challenge_url;
                    Ok((test, client_dpop_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }
}

#[cfg(not(ci))]
mod oidc_challenge {
    use super::*;

    /// Authorization Server (Dex in our case) exposes an endpoint for clients to fetch its public keys.
    /// It is used to validate the signature of the id token we supply to this challenge.
    #[tokio::test]
    async fn should_fail_when_oidc_provider_jwks_uri_unavailable() {
        let mut test = E2eTest::new();
        // invalid jwks uri
        let mut jwks_uri: url::Url = test.ca_cfg.jwks_uri.parse().unwrap();
        jwks_uri.set_port(Some(jwks_uri.port().unwrap() + 1)).unwrap();
        test.ca_cfg.jwks_uri = jwks_uri.to_string();
        let test = test.start(docker()).await;

        // cannot validate the OIDC challenge
        assert!(matches!(
            test.nominal_enrollment().await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ClientImplementationError(
                "a challenge is not supposed to be pending at this point. It must either be 'valid' or 'processing'."
            ))
        ));
    }

    /// Authorization Server (Dex in our case) exposes an endpoint for clients to fetch its public keys.
    /// It is used to validate the signature of the id token we supply to this challenge.
    /// Here, the AS will return a valid JWKS URI but it contains an invalid public key
    /// for verifying the id token.
    #[tokio::test]
    async fn should_fail_when_malicious_jwks_uri() {
        let docker = docker();

        let mut test = E2eTest::new();
        let (jwks_stub, ..) = test.new_jwks_uri_mock();
        // this starts a server serving the abose stub with a malicious JWK
        let attacker_host = "attacker-keycloak";
        let _attacker_keycloak = WiremockImage::run(docker, attacker_host, vec![jwks_stub]);

        // invalid jwks uri
        test.ca_cfg.jwks_uri = format!("http://{attacker_host}/oauth2/jwks");
        let test = test.start(docker).await;

        // cannot validate the OIDC challenge
        assert!(matches!(
            test.nominal_enrollment().await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ClientImplementationError(
                "a challenge is not supposed to be pending at this point. It must either be 'valid' or 'processing'."
            ))
        ));
    }

    /// An id token with an invalid name is supplied to ACME server. It should verify that the handle
    /// is the same as the one used in the order.
    #[tokio::test]
    #[ignore] // FIXME: adapt with Keycloak
    async fn should_fail_when_invalid_handle() {
        let docker = docker();
        let mut test = E2eTest::new();

        // setup fake jwks_uri to be able to resign the id token
        let (jwks_stub, new_kp, kid) = test.new_jwks_uri_mock();
        let attacker_host = "attacker-keycloak";
        let _attacker_keycloak = WiremockImage::run(docker, attacker_host, vec![jwks_stub]);
        test.ca_cfg.jwks_uri = format!("https://{attacker_host}/realms/master/protocol/openid-connect/certs");

        let test = test.start(docker).await;

        let flow = EnrollmentFlow {
            fetch_id_token: Box::new(|mut test, (oidc_chall, keyauth)| {
                Box::pin(async move {
                    let idp_pk = test.fetch_idp_public_key().await;
                    let dex_pk = RS256PublicKey::from_pem(&idp_pk).unwrap();
                    let id_token = test.fetch_id_token(&oidc_chall, keyauth).await?;

                    let change_handle = |mut claims: JWTClaims<Value>| {
                        let wrong_handle = format!("{}john.doe.qa@wire.com", ClientId::URI_SCHEME);
                        *claims.custom.get_mut("name").unwrap() = json!(wrong_handle);
                        claims
                    };
                    let modified_id_token = resign_id_token(&id_token, dex_pk, kid, new_kp, change_handle);
                    Ok((test, modified_id_token))
                })
            }),
            ..Default::default()
        };

        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ClientImplementationError(
                "a challenge is not supposed to be pending at this point. It must either be 'valid' or 'processing'."
            ))
        ));
    }

    /// An id token with an invalid name is supplied to ACME server. It should verify that the display name
    /// is the same as the one used in the order.
    #[tokio::test]
    #[ignore] // FIXME: adapt with Keycloak
    async fn should_fail_when_invalid_display_name() {
        let docker = docker();
        let mut test = E2eTest::new();

        // setup fake jwks_uri to be able to resign the id token
        let (jwks_stub, new_kp, kid) = test.new_jwks_uri_mock();
        let attacker_host = "attacker-dex";
        let _attacker_dex = WiremockImage::run(docker, attacker_host, vec![jwks_stub]);
        test.ca_cfg.jwks_uri = format!("https://{attacker_host}/realms/master/protocol/openid-connect/certs");

        let test = test.start(docker).await;

        let flow = EnrollmentFlow {
            fetch_id_token: Box::new(|mut test, (oidc_chall, keyauth)| {
                Box::pin(async move {
                    let dex_pk = test.fetch_idp_public_key().await;
                    let dex_pk = RS256PublicKey::from_pem(&dex_pk).unwrap();
                    let id_token = test.fetch_id_token(&oidc_chall, keyauth).await?;

                    let change_handle = |mut claims: JWTClaims<Value>| {
                        let wrong_handle = "Doe, John (QA)";
                        *claims.custom.get_mut("preferred_username").unwrap() = json!(wrong_handle);
                        claims
                    };
                    let modified_id_token = resign_id_token(&id_token, dex_pk, kid, new_kp, change_handle);
                    Ok((test, modified_id_token))
                })
            }),
            ..Default::default()
        };

        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ClientImplementationError(
                "a challenge is not supposed to be pending at this point. It must either be 'valid' or 'processing'."
            ))
        ));
    }

    /// We use the "keyauth": '{oidc-challenge-token}.{acme-key-thumbprint}' to bind the acme client to the id token
    /// we validate in the acme server. This prevents id token being stolen or OAuth authorization performed outside of
    /// the current ACME session.
    #[tokio::test]
    async fn should_fail_when_invalid_keyauth() {
        let test = E2eTest::new().start(docker()).await;
        let flow = EnrollmentFlow {
            fetch_id_token: Box::new(|mut test, (oidc_chall, _keyauth)| {
                Box::pin(async move {
                    let keyauth = rand_base64_str(32); // a random 'keyauth'
                    let id_token = test.fetch_id_token(&oidc_chall, keyauth).await?;
                    Ok((test, id_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }

    /// We add a "acme_aud" in the idToken which must match the OIDC challenge url
    #[tokio::test]
    async fn should_fail_when_invalid_audience() {
        let test = E2eTest::new().start(docker()).await;
        let flow = EnrollmentFlow {
            fetch_id_token: Box::new(|mut test, (mut oidc_chall, keyauth)| {
                Box::pin(async move {
                    // alter the challenge url to alter the idToken audience, then restore the challenge url
                    let backup_oidc_challenge_url = oidc_chall.url.clone();
                    oidc_chall.url = "http://unknown.com".parse().unwrap();

                    let id_token = test.fetch_id_token(&oidc_chall, keyauth).await?;
                    oidc_chall.url = backup_oidc_challenge_url;
                    Ok((test, id_token))
                })
            }),
            ..Default::default()
        };
        assert!(matches!(
            test.enrollment(flow).await.unwrap_err(),
            TestError::Acme(RustyAcmeError::ChallengeError(AcmeChallError::Invalid))
        ));
    }
}
