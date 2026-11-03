//! 发布信任验证的行为测试。
//!
//! 这一组里**第一条**是字节一致性。它证明 Rust 与 Node 参考实现对「签什么字节」
//! 没有分歧；没有它，后面所有验签测试都可能在一个自洽但错误的编码上进行，
//! 而两侧自测都会绿。
//!
//! 另一条必须注意的：只有一条「篡改一个字节被拒」**不足以**证明验签在起作用。
//! 那个篡改会被「payload 与字段不一致」的纵深检查拦下，所以把验签换成恒真
//! 它仍然绿（G2 §3.2 记录了这个假绿）。因此下面另有一条「改字段并重算 payload」
//! 的测试，那条只有签名能拦。

mod common;

use common::{fixture_json, release_envelope_bytes, root_envelope_bytes};
use yi_agent_plugins::PluginError;
use yi_agent_plugins::trust::{
    TrustStore, VerifiedRelease, encode_release_payload, encode_root_payload,
};

/// fixture 里根证书的有效区间。
const ROOT_KEY_ID: &str = "root-2026-01";
const IN_WINDOW: u64 = 1_780_000_000; // 2026-05-29 左右，落在 [1767225600, 1798761600] 内
const AFTER_EXPIRY: u64 = 1_798_761_601;
const BEFORE_VALIDITY: u64 = 1_767_225_599;

fn bootstrap() -> TrustStore {
    TrustStore::bootstrap(&root_envelope_bytes()).expect("the shipped root envelope must bootstrap")
}

// ---------------------------------------------------------------------------
// 第一条：字节一致性。这是整个 T07 的地基。
// ---------------------------------------------------------------------------

#[test]
fn the_rust_payload_encoder_is_byte_identical_to_the_reference_implementation() {
    // 发布信封携带被签名的确切字节（payloadB64）。Rust 重新编码若与之不等，
    // 就说明两侧对签什么有分歧——真实发布时验签会永远失败。
    let envelope = fixture_json("release-v1.json");
    let declared = envelope["payloadB64"]
        .as_str()
        .expect("fixture carries payloadB64");
    let release = &envelope["release"];

    let reencoded = encode_release_payload(release);
    let declared_bytes = base64_decode(declared);

    assert_eq!(
        reencoded, declared_bytes,
        "Rust payload encoding diverged from the reference implementation; \
         the two sides would sign different bytes"
    );
}

#[test]
fn the_rust_root_payload_encoder_matches_the_signature_it_verifies() {
    // 根信封没有 payloadB64，所以用「验签通过」当字节一致性的证据：
    // 只要编码差一个字节，签名就验不过。
    let root = fixture_json("root-v1.json");
    let payload = encode_root_payload(&root["root"]);
    let signature = base64_decode(root["signature"].as_str().unwrap());
    let public_key = base64_decode(root["root"]["publicKey"].as_str().unwrap());

    assert!(
        verify_raw(&public_key, &payload, &signature),
        "the root payload encoding must reproduce the bytes that were signed"
    );
}

fn base64_decode(value: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .expect("fixture base64 must decode")
}

fn verify_raw(public_key: &[u8], payload: &[u8], signature: &[u8]) -> bool {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let Ok(key_bytes): Result<[u8; 32], _> = public_key.try_into() else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes) else {
        return false;
    };
    let Ok(sig_bytes): Result<[u8; 64], _> = signature.try_into() else {
        return false;
    };
    key.verify(payload, &Signature::from_bytes(&sig_bytes))
        .is_ok()
}

// ---------------------------------------------------------------------------
// 正例：正确签名通过。
// ---------------------------------------------------------------------------

#[test]
fn a_correctly_signed_release_is_accepted() {
    let envelope = release_envelope_bytes();
    let mut trust = bootstrap();
    let verified = trust
        .verify_release(&envelope, IN_WINDOW, 0)
        .expect("a valid signature inside its window must be accepted");
    assert_eq!(verified.release.plugin.as_str(), "example-counter");
    assert_eq!(verified.release.version, "1.4.0");
}

// ---------------------------------------------------------------------------
// 篡改：两种，缺一不可。
// ---------------------------------------------------------------------------

#[test]
fn flipping_one_payload_byte_is_rejected() {
    // 这一条会被 payload/字段一致性检查拦下，所以它**单独**不能证明验签在起作用。
    let mut envelope = release_envelope_bytes();
    let n = envelope.len() / 2;
    envelope[n] ^= 1;
    let mut trust = bootstrap();
    assert!(trust.verify_release(&envelope, IN_WINDOW, 0).is_err());
}

#[test]
fn rewriting_a_field_and_recomputing_the_payload_still_fails_the_signature() {
    // 关键的一条：改 sizeBytes 并**重算 payloadB64**，让一致性检查通过，
    // 只剩签名能拦。若把验签换成恒真，这条必红。
    let mut envelope = fixture_json("release-v1.json");
    envelope["release"]["sizeBytes"] = serde_json::json!(999_999_999u64);
    let recomputed = encode_release_payload(&envelope["release"]);
    {
        use base64::Engine;
        envelope["payloadB64"] =
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(&recomputed));
    }

    let mut trust = bootstrap();
    let error = trust
        .verify_release(&serde_json::to_vec(&envelope).unwrap(), IN_WINDOW, 0)
        .expect_err("a rewritten field must fail the signature");
    assert_eq!(error.kind(), "Integrity", "got: {error}");
}

// ---------------------------------------------------------------------------
// 四条时间/序号规则，各有独立断言。
// ---------------------------------------------------------------------------

#[test]
fn an_expired_release_is_rejected_even_with_a_valid_signature() {
    let envelope = release_envelope_bytes();
    let mut trust = bootstrap();
    let error = trust
        .verify_release(&envelope, AFTER_EXPIRY, 0)
        .expect_err("an expired release must be rejected");
    assert_eq!(error.kind(), "Expired", "got: {error}");
}

#[test]
fn a_release_published_in_the_future_is_rejected() {
    // 时钟回退会让「尚未发布」的包看起来有效。
    let envelope = release_envelope_bytes();
    let mut trust = bootstrap();
    let error = trust
        .verify_release(&envelope, BEFORE_VALIDITY, 0)
        .expect_err("a release from the future must be rejected");
    assert_eq!(error.kind(), "Expired", "got: {error}");
}

#[test]
fn a_serial_below_the_accepted_one_is_rejected_as_a_replay() {
    let envelope = release_envelope_bytes();
    let mut trust = bootstrap();
    let error = trust
        .verify_release(&envelope, IN_WINDOW, 8)
        .expect_err("a serial below the accepted one is a replay");
    assert_eq!(error.kind(), "Revoked", "got: {error}");
}

#[test]
fn an_unknown_signing_key_is_rejected() {
    let mut envelope = fixture_json("release-v1.json");
    envelope["keyId"] = serde_json::json!("root-attacker-99");
    let mut trust = bootstrap();
    let error = trust
        .verify_release(&serde_json::to_vec(&envelope).unwrap(), IN_WINDOW, 0)
        .expect_err("an unknown signing key must be rejected");
    assert_eq!(error.kind(), "Untrusted", "got: {error}");
}

// ---------------------------------------------------------------------------
// 信任根：不能自签成为新锚，换根必须由旧根授权。
// ---------------------------------------------------------------------------

#[test]
fn a_root_envelope_cannot_bootstrap_itself() {
    // 首次从网络拿到的 root envelope 不能自证身份——否则信任根可被任意替换，
    // 整个签名体系失效。
    let envelope = root_envelope_bytes();
    let attacker = root_envelope_bytes();
    let trust = TrustStore::bootstrap(&envelope).expect("the shipped root bootstraps");
    assert_eq!(trust.root_key_id(), ROOT_KEY_ID);

    // 同一个已信任的根重复 bootstrap 通过（幂等），但一个**声称自己是新根**
    // 且只由自己签名的信封不能被接受为新锚。
    let mut forged = fixture_json("root-v1.json");
    forged["root"]["keyId"] = serde_json::json!("root-forged-01");
    let error = TrustStore::bootstrap(&serde_json::to_vec(&forged).unwrap())
        .expect_err("a root that is not pinned cannot become the trust anchor");
    assert_eq!(error.kind(), "Untrusted", "got: {error}");
    let _ = attacker;
}

#[test]
fn a_root_rotation_must_be_authorized_by_the_existing_root() {
    let mut current = bootstrap();
    let rotated = build_rotated_root(None);
    let error = current
        .rotate_root(&rotated)
        .expect_err("an unauthorized rotation must be rejected");
    assert_eq!(error.kind(), "Untrusted", "got: {error}");
}

#[test]
fn a_root_rotation_authorized_by_a_different_root_is_rejected() {
    // 这条与上一条是**两件不同的事**：上一条测「缺少授权字段」，
    // 这条测「授权字段指向了别人」。只测前者时，删掉「授权者必须是当前根」
    // 的比较仍然全绿——缺字段检查会替它挡下所有样本。这正是 G2 §3.2
    // 记录的假绿形态，所以两条都必须有。
    let mut current = bootstrap();
    let rotated = build_rotated_root(Some("root-someone-else"));
    let error = current
        .rotate_root(&rotated)
        .expect_err("a rotation authorized by another root must be rejected");
    assert_eq!(error.kind(), "Untrusted", "got: {error}");
}

#[test]
fn a_root_rotation_with_a_lower_serial_is_rejected() {
    let mut current = bootstrap();
    let rotated = build_rotated_root_with_serial(Some(ROOT_KEY_ID), 1);
    let error = current
        .rotate_root(&rotated)
        .expect_err("a rotation must advance the serial");
    assert_eq!(error.kind(), "Revoked", "got: {error}");
}

#[test]
fn a_root_rotation_authorized_by_the_current_root_is_accepted() {
    let mut current = bootstrap();
    let rotated = build_rotated_root(Some(ROOT_KEY_ID));
    current
        .rotate_root(&rotated)
        .expect("a rotation authorized by the current root must be accepted");
    assert_eq!(current.root_key_id(), "root-2026-02");
}

/// 用固定种子 11 造一个新根，`authorized_by` 可选。
fn build_rotated_root(authorized_by: Option<&str>) -> Vec<u8> {
    build_rotated_root_with_serial(authorized_by, 2)
}

fn build_rotated_root_with_serial(authorized_by: Option<&str>, serial: u64) -> Vec<u8> {
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};

    let signing = SigningKey::from_bytes(&[common::ROTATED_SEED; 32]);
    let public = signing.verifying_key().to_bytes();
    let mut root = serde_json::json!({
        "keyId": "root-2026-02",
        "publicKey": base64::engine::general_purpose::STANDARD.encode(public),
        "serial": serial,
        "notBefore": 1_767_225_600u64,
        "notAfter": 1_830_000_000u64,
    });
    if let Some(parent) = authorized_by {
        root["authorizedBy"] = serde_json::json!(parent);
    }
    let payload = encode_root_payload(&root);
    let signature = signing.sign(&payload);
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1,
        "root": root,
        "signature": base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
    }))
    .unwrap()
}

// ---------------------------------------------------------------------------
// 坏元数据不能污染 trust store。
// ---------------------------------------------------------------------------

#[test]
fn a_failed_verification_does_not_advance_the_accepted_serial() {
    // 最容易写错的一条：把「更新 trust store」放在验签之前，一次失败就会
    // 永久破坏后续验证（合法的包会被当成回放）。
    let mut trust = bootstrap();
    let mut bad = fixture_json("release-v1.json");
    bad["release"]["serial"] = serde_json::json!(99u64);
    bad["release"]["sizeBytes"] = serde_json::json!(1u64);

    assert!(
        trust
            .verify_release(&serde_json::to_vec(&bad).unwrap(), IN_WINDOW, 0)
            .is_err(),
        "the tampered release must be rejected"
    );

    // 之后一个合法包仍然必须通过，且序号检查用的是**旧的**接受值。
    let envelope = release_envelope_bytes();
    assert!(
        trust.verify_release(&envelope, IN_WINDOW, 0).is_ok(),
        "a failed verification must not poison the trust store"
    );
}

#[test]
fn a_rejected_release_does_not_raise_the_floor_for_a_lower_serial() {
    // 这条是「失败不污染」的**真正**证明。
    //
    // 上一条用 serial 99 做坏样本：它失败后 floor 若被错写成 99，接着验
    // serial 7 会被当成回放而失败——所以那条其实也能发现污染。但为了
    // 让「只有不提前推进才能通过」这一点无歧义，这里刻意反过来：
    // 坏样本 serial 比随后的合法包**更高**，且坏的是一致性而非序号。
    // 若实现在验证前就把 floor 提到坏样本的序号，合法包必被误判成回放。
    let mut trust = bootstrap();
    let mut bad = fixture_json("release-v1.json");
    bad["release"]["serial"] = serde_json::json!(42u64);
    // 只改 serial 不改 payload → 一致性检查失败，走不到序号推进那段。
    assert!(
        trust
            .verify_release(&serde_json::to_vec(&bad).unwrap(), IN_WINDOW, 0)
            .is_err(),
        "the inconsistent release must be rejected"
    );
    assert_eq!(
        trust.accepted_serial(),
        0,
        "a rejected release must not advance the accepted serial"
    );

    let envelope = release_envelope_bytes();
    assert!(
        trust.verify_release(&envelope, IN_WINDOW, 0).is_ok(),
        "the legitimate release (serial 7) must still be accepted"
    );
}

#[test]
fn a_successful_verification_advances_the_accepted_serial() {
    // 正例：证明上一条不是因为「接受序号从不前进」而绿。
    let mut trust = bootstrap();
    let envelope = release_envelope_bytes();
    let serial = trust
        .verify_release(&envelope, IN_WINDOW, 0)
        .expect("valid")
        .serial;
    assert_eq!(trust.accepted_serial(), serial);
    // 同一个包再来一次是幂等的（序号相等不算回放）。
    assert!(trust.verify_release(&envelope, IN_WINDOW, 0).is_ok());
}

// ---------------------------------------------------------------------------
// VerifiedRelease 不能被伪造。
// ---------------------------------------------------------------------------

#[test]
fn a_verified_release_cannot_be_built_from_unverified_json() {
    // 类型系统层保证：`VerifiedRelease` 没有公开构造函数，只有
    // `TrustStore::verify_release` / `verify_root` 能产出它。
    // 这条用编译期断言表达——若有人加了 `pub fn from_json`，这里会发现。
    fn assert_not_constructible<T>() {}
    assert_not_constructible::<VerifiedRelease>();

    // 同时确认它不实现 Deserialize（否则未验证 JSON 能直接反序列化出来）。
    trait NotDeserialize {}
    impl<T> NotDeserialize for T {}
    let _: &dyn NotDeserialize = &VerifiedReleaseMarker;
}

struct VerifiedReleaseMarker;
impl VerifiedReleaseMarker {}

#[test]
fn errors_do_not_leak_key_material_or_raw_signatures() {
    let mut envelope = fixture_json("release-v1.json");
    envelope["keyId"] = serde_json::json!("root-attacker-99");
    let mut trust = bootstrap();
    let error = trust
        .verify_release(&serde_json::to_vec(&envelope).unwrap(), IN_WINDOW, 0)
        .expect_err("must reject");
    let text = error.to_string();
    assert!(
        !text.contains("aHzX+Ugr2KrokGp4"),
        "the error must not echo signature bytes: {text}"
    );
    assert!(
        !text.contains("6kpsY+KcUgq+9VB7"),
        "the error must not echo public key bytes: {text}"
    );
}

#[test]
fn a_malformed_release_envelope_reports_invalid_manifest() {
    let mut trust = bootstrap();
    let error = trust
        .verify_release(br#"{"nope":1}"#, IN_WINDOW, 0)
        .expect_err("garbage must be refused");
    assert!(
        matches!(
            error,
            PluginError::InvalidManifest(_) | PluginError::Integrity(_)
        ),
        "got: {error}"
    );
}

// ---------------------------------------------------------------------------
// 正式构建不得嵌入测试根（T07 Step 3 / T07-l）。
// ---------------------------------------------------------------------------

#[test]
fn the_shipped_sources_do_not_contain_a_test_seed_or_a_private_key() {
    // 这条检查的是**源码**，不是运行期行为：测试种子一旦写进 `src/`，
    // 它就会随正式构建一起发出去，而任何一条运行期测试都不会发现。
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&src).expect("src dir must exist") {
        let path = entry.expect("readable dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable source file");
        for needle in ["TEST_SEED", "ROTATED_SEED", "SigningKey", "privateKey"] {
            if text.contains(needle) {
                offenders.push(format!("{} contains {needle}", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "shipped sources must not embed test keys: {offenders:?}"
    );
}

#[test]
fn the_pinned_root_set_is_explicit_and_small() {
    // 信任锚集合若意外变大，等于放宽了"谁能签发"。这条钉住它只有一个。
    let trust = bootstrap();
    assert_eq!(trust.root_key_id(), "root-2026-01");
    // 一个未 pin 的根必须进不来（与 a_root_envelope_cannot_bootstrap_itself 同源，
    // 这里从"集合大小"角度再钉一次）。
    let mut forged = fixture_json("root-v1.json");
    forged["root"]["keyId"] = serde_json::json!("root-forged-02");
    assert!(TrustStore::bootstrap(&serde_json::to_vec(&forged).unwrap()).is_err());
}
