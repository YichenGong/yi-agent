use super::*;

/// 用户从工作区外选中的文件必须被**复制**进 thread 的 cwd，元数据回显在开启项
/// 上；气泡正文仍是用户原话，注入的清单只出现在送给模型的 prompt 里。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_copies_attachments_and_echoes_them_on_the_item() {
    // 用自带 workdir 的 harness：默认 `/tmp` 目录会跨次运行残留同名附件，
    // 那样「盘上存在」就可能被旧文件蒙混过关。
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    // 源文件放在工作区外，模拟「从 Downloads 选文件」。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("报告.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"总结一下"}}]}}}}"#,
        src.display()
    ))
    .await;

    // 本轮第一条 item/started 就是开启项:用户消息(附 attachment 元数据)。
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["attachments"][0]["name"], "报告.pdf", "{item}");
    let rel = item["attachments"][0]["path"]
        .as_str()
        .expect("an attachment path must be a string");
    assert!(rel.starts_with(".yi-agent/attachments/"), "{rel}");
    // 磁盘上确实复制成功（相对 thread 的 cwd = 该 harness 的 workdir）。
    assert!(
        workdir.path().join(rel).is_file(),
        "attachment was not copied: {rel}"
    );
    // 气泡正文是用户原话，不含注入的清单。
    assert_eq!(item["text"], "总结一下", "{item}");
    h.shutdown().await;
}

/// 反向断言：清单必须真的送进**模型**。只断言 `Item.text` 不含清单一事
/// 无法防住「将来把 `display_text` 传给 `agent.run`」这类静默回归。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_prompt_reaching_the_model_contains_the_attachment_manifest() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();

    let prompts: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let prompts_factory = Arc::clone(&prompts);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CapturingProvider {
            prompts: Arc::clone(&prompts_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            mcp: None,
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("报告.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"总结一下"}}]}}}}"#,
        src.display()
    ))
    .await;

    // 等到 turn 收尾，确保 provider 至少被调用过一次。
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        if opener.is_none()
            && v.get("method").and_then(|m| m.as_str()) == Some("item/started")
            && v["params"]["item"]["type"] == "userMessage"
        {
            opener = Some(v["params"]["item"].clone());
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    // 送给模型的那条 user 文本必须带清单（安全化 + 加引号后的形态）。
    let seen = prompts.lock().unwrap().clone();
    let sent = seen
        .last()
        .expect("the provider must have been called at least once");
    assert!(
        sent.contains("Attached files (read them with read_document):"),
        "the manifest header never reached the model: {sent:?}"
    );
    assert!(
        sent.contains("- \"报告.pdf\" (application/pdf, 13 bytes) -> .yi-agent/attachments/"),
        "the sanitized, quoted manifest line is missing from the model prompt: {sent:?}"
    );
    assert!(
        sent.starts_with("总结一下"),
        "the model prompt must still lead with the user's words: {sent:?}"
    );

    // 气泡/Item.text 仍只是用户原话——清单不进 UI 文本。
    let item = opener.expect("the turn must open with a user item");
    assert_eq!(item["text"], "总结一下", "{item}");
    h.shutdown().await;
}

/// 桌面端从「图片」选择器选中的图（工作区外的绝对路径）必须被复制进 thread
/// 的 cwd，元数据以 `ImageRef` 回显在开启项上；气泡正文仍是用户原话，
/// 图片**不进**附件清单（它是内容块，不是文档附件）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_with_an_image_injects_an_image_block_and_an_item_ref() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一张 8x8 PNG，放在工作区外（模拟从「图片」选择器选）。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("shot.png");
    let img = ::image::RgbImage::from_fn(8, 8, |_, _| ::image::Rgb([1, 2, 3]));
    img.save_with_format(&src, ::image::ImageFormat::Png)
        .unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}},{{"type":"text","text":"这是什么"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["text"], "这是什么", "{item}");
    let rel = item["images"][0]["path"].as_str().expect("image path");
    assert!(rel.starts_with(".yi-agent/attachments/"), "{rel}");
    assert!(
        workdir.path().join(rel).is_file(),
        "image not copied: {rel}"
    );
    assert_eq!(item["images"][0]["media_type"], "image/png", "{item}");
    assert_eq!(item["images"][0]["detail"], "high", "{item}");
    // 气泡文本里不得注入清单。
    assert!(
        !item["text"].as_str().unwrap().contains("Attached files"),
        "{item}"
    );

    // 落盘回合里也必须带上 images 引用：resume 回放时气泡要能重新渲染出图。
    let _ = h.read_until_turn_completed().await;
    let jsonl = std::fs::read_to_string(
        workdir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{tid}.jsonl")),
    )
    .expect("the thread log must exist after a completed turn");
    let line: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    let opened = &line["items"][0];
    assert_eq!(opened["type"], "userMessage", "{line}");
    assert_eq!(opened["images"][0]["path"], rel, "{line}");
    assert_eq!(opened["images"][0]["size"], src.metadata().unwrap().len());
    h.shutdown().await;
}

/// 回归：`images[].size` 必须取**存盘文件**的大小，而不是重编码后载荷的长度。
///
/// 上面那条测试用的是 8x8 直通 PNG：`prepare_image_file` 原样返回源字节，
/// 于是「载荷长度」与「文件大小」恰好相等，即使实现数错了也照样通过。
/// 真正会暴露问题的是**被重编码**的图：这里用一张宽 4096px 的 PNG，`High`
/// 档（最长边 2048）必然触发降采样重编码，载荷与文件大小不再相等。
///
/// 同时断言落盘的 `.jsonl` 开启项与实时 `item/started` 开启项**是同一个数**：
/// 两者若各算各的（一个读文件、一个量载荷）就会分叉，这正是被修复的缺陷。
#[tokio::test(flavor = "multi_thread")]
async fn a_re_encoded_image_persists_the_stored_file_size() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 宽 4096、高 64 的 PNG：最长边超 2048，`High` 档必降采样重编码；
    // 且整幅尺寸很小，编码后仍远低于字节预算，所以走的就是「降采样 + 重编码」
    // 这条（而不是字节超限才降级的）路径。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("wide.png");
    let img = ::image::RgbImage::from_fn(4096, 64, |x, y| {
        ::image::Rgb([(x % 251) as u8, (y % 253) as u8, ((x ^ y) % 255) as u8])
    });
    img.save_with_format(&src, ::image::ImageFormat::Png)
        .unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}},{{"type":"text","text":"看看这张"}}]}}}}"#,
        src.display()
    ))
    .await;

    // 实时开启项：`size` 必须等于存盘文件的大小（重编码不影响它）。
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    let rel = item["images"][0]["path"]
        .as_str()
        .expect("image path")
        .to_string();
    let stored = workdir.path().join(&rel);
    let stored_len = stored.metadata().expect("the stored copy must exist").len();

    // 前提检查：这张图确实被 image_prep 重编码了，否则本条测试抓不到缺陷。
    //
    // 存盘文件是源字节的**逐字副本**，所以文件大小永远等于源大小；重编码的
    // 结果只存在于内存里的 base64 载荷。这里用同一个公开入口复算一次载荷，
    // 证实「载荷长度 != 文件大小」，于是「缺了这段元数据就只能去量载荷」的
    // 老实现必然落在这个数上。
    let payload_len = match yi_agent_tools::image_prep::prepare_image_file(
        &stored,
        yi_agent_core::ImageDetail::High,
        yi_agent_tools::image_prep::resolve_budget(),
    )
    .expect("the fixture must be a readable image")
    {
        yi_agent_tools::image_prep::PreparedImage::Ready {
            data, width, note, ..
        } => {
            assert!(
                width < 4096,
                "the fixture must be downscaled (width {width}) for this test to be meaningful"
            );
            assert!(
                note.is_some(),
                "the fixture must be re-encoded (no note means pass-through)"
            );
            let b64 = data.as_bytes();
            let padding = b64.iter().rev().take_while(|b| **b == b'=').count().min(2);
            ((b64.len() / 4) * 3 - padding) as u64
        }
        other => panic!("expected a Ready image, got {other:?}"),
    };
    assert_ne!(
        payload_len, stored_len,
        "the fixture must actually be re-encoded for this test to be meaningful"
    );
    assert_eq!(
        item["images"][0]["size"].as_u64(),
        Some(stored_len),
        "the live opener must report the stored file size, not the payload length: {item}"
    );

    // 落盘开启项：与实时开启项必须是同一个数（一份来源）。
    let _ = h.read_until_turn_completed().await;
    let jsonl = std::fs::read_to_string(
        workdir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{tid}.jsonl")),
    )
    .expect("the thread log must exist after a completed turn");
    let line: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    let opened = &line["items"][0];
    assert_eq!(opened["images"][0]["path"], rel.as_str(), "{opened}");
    assert_eq!(
        opened["images"][0]["size"].as_u64(),
        Some(stored_len),
        "the persisted opener must report the stored file size: {opened}"
    );
    assert_eq!(
        opened["images"][0]["size"], item["images"][0]["size"],
        "the persisted and live openers must agree: {opened} vs {item}"
    );
    h.shutdown().await;
}

/// iOS 分片上传的完整往返:`begin → chunk(0..n) → commit → turn/start`。
///
/// 断言三件事,每件都是这条 RPC 集结的一条不变量:
/// - `commit` 返回的 `path`/`size` 就是**存盘文件**本身(`size` == 该文件字节长度);
/// - 存盘内容逐字节等于分块拼接;staging 已被清掉;
/// - `turn/start` 用 `{type:"uploaded_image", uploadId}` 摄取后,开启项
///   `images[0].size` 是**文件长度**而非 base64 载荷长度(用真正被重编码的
///   夹具把两者分开),且落盘 `.jsonl` 与实时开启项报同一个数。
#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_image_round_trips_through_commit_and_turn_start() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let bytes = write_a_re_encodable_png_into_memory();
    let total = bytes.len() as u64;

    let begin = rpc(
        &mut h,
        20,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid, "name": "shot.png", "mime": "image/png", "size": total
        }),
    )
    .await;
    assert!(begin.get("error").is_none(), "{begin}");
    let upload_id = begin["result"]["uploadId"]
        .as_str()
        .expect("begin must return an uploadId")
        .to_string();
    assert!(
        begin["result"]["chunkSize"].as_u64().unwrap_or(0) > 0,
        "{begin}"
    );

    // 切三块发(真实客户端也分批送);块序从 0 起递增。
    let step = bytes.len().div_ceil(3);
    for (i, part) in bytes.chunks(step).enumerate() {
        let data = base64::engine::general_purpose::STANDARD.encode(part);
        let resp = rpc(
            &mut h,
            21 + i as u64,
            "image/upload/chunk",
            serde_json::json!({ "uploadId": upload_id, "index": i, "data": data }),
        )
        .await;
        assert!(resp.get("error").is_none(), "chunk {i} refused: {resp}");
    }

    let commit = rpc(
        &mut h,
        30,
        "image/upload/commit",
        serde_json::json!({ "uploadId": upload_id }),
    )
    .await;
    assert!(commit.get("error").is_none(), "{commit}");
    let rel = commit["result"]["path"]
        .as_str()
        .expect("commit must return the stored path")
        .to_string();
    assert!(
        rel.starts_with(&format!(".yi-agent/attachments/{tid}/")),
        "{rel}"
    );
    assert!(
        rel.ends_with("-shot.png"),
        "commit must keep the client's name: {rel}"
    );
    assert_eq!(commit["result"]["mediaType"], "image/png", "{commit}");
    let stored = workdir.path().join(&rel);
    let stored_len = stored.metadata().expect("the stored copy must exist").len();
    assert_eq!(
        commit["result"]["size"].as_u64(),
        Some(stored_len),
        "{commit}"
    );
    assert_eq!(
        std::fs::read(&stored).unwrap(),
        bytes,
        "the stored copy must be the concatenated chunks"
    );
    // staging 必须已被清掉。
    assert!(
        !workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid)
            .join(".tmp")
            .join(&upload_id)
            .exists(),
        "the staging file must be gone after commit"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":31,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"uploaded_image","uploadId":"{upload_id}"}},{{"type":"text","text":"看看这张"}}]}}}}"#
    ))
    .await;

    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["text"], "看看这张", "{item}");
    assert_eq!(item["images"][0]["path"], rel.as_str(), "{item}");
    assert_eq!(item["images"][0]["media_type"], "image/png", "{item}");
    assert_eq!(
        item["images"][0]["size"].as_u64(),
        Some(stored_len),
        "the live opener must report the stored file size: {item}"
    );
    // 前提检查:这张图确实被重编码了,否则「size == 文件长度」会因为两者相等
    // 而抓不到「误用载荷长度」的实现。顺便把载荷本身拿出来,做「Item 里不含
    // base64」的反向断言。
    let payload_base64 = match yi_agent_tools::image_prep::prepare_image_file(
        &stored,
        yi_agent_core::ImageDetail::High,
        yi_agent_tools::image_prep::resolve_budget(),
    )
    .expect("the fixture must be a readable image")
    {
        yi_agent_tools::image_prep::PreparedImage::Ready { data, .. } => data,
        other => panic!("expected a Ready image, got {other:?}"),
    };
    let b64 = payload_base64.as_bytes();
    let padding = b64.iter().rev().take_while(|b| **b == b'=').count().min(2);
    let payload_len = ((b64.len() / 4) * 3 - padding) as u64;
    assert_ne!(
        payload_len, stored_len,
        "the fixture must be re-encoded for this test to be meaningful"
    );

    // 落盘开启项与实时开启项必须同源(同一个数)。
    let _ = h.read_until_turn_completed().await;
    let jsonl = std::fs::read_to_string(
        workdir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{tid}.jsonl")),
    )
    .expect("the thread log must exist after a completed turn");
    let line: serde_json::Value = serde_json::from_str(jsonl.lines().next().unwrap()).unwrap();
    let opened = &line["items"][0];
    assert_eq!(opened["images"][0]["path"], rel.as_str(), "{opened}");
    assert_eq!(
        opened["images"][0]["size"].as_u64(),
        Some(stored_len),
        "the persisted opener must report the stored file size: {opened}"
    );
    // Item 里绝不能出现 base64 载荷:item 是每条通知都带的东西,内联图片字节
    // 会撑爆 1 MiB 的帧(设计 §3 的关键约束)。断言只针对**落盘的 items**:
    // 同一行里的 `messages` 是原样留存的会话(供 resume),其中的内容块本就带
    // base64,那是 Task 5 既有且必需的行为。
    assert!(
        !serde_json::to_string(&line["items"])
            .unwrap()
            .contains(&payload_base64),
        "no base64 may ever appear in an Item"
    );
    h.shutdown().await;
}

/// 反向断言:上传路径也必须真的把图送进**模型**(内容块),而不是只回显 item。
/// 与桌面 `{type:"image", path}` 落到同一条 `prepare_image_file` 管线(R3)。
#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_image_reaches_the_model_as_a_content_block() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();

    let messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let messages_factory = Arc::clone(&messages);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CapturingMessagesProvider {
            messages: Arc::clone(&messages_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            mcp: None,
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let img = ::image::RgbImage::from_fn(8, 8, |_, _| ::image::Rgb([9, 9, 9]));
    let mut bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        ::image::ImageFormat::Png,
    )
    .unwrap();

    let begin = rpc(
        &mut h,
        40,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid, "name": "shot.png", "mime": "image/png", "size": bytes.len(),
        }),
    )
    .await;
    let upload_id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    rpc(
        &mut h,
        41,
        "image/upload/chunk",
        serde_json::json!({ "uploadId": upload_id, "index": 0, "data": data }),
    )
    .await;
    let commit = rpc(
        &mut h,
        42,
        "image/upload/commit",
        serde_json::json!({ "uploadId": upload_id }),
    )
    .await;
    let rel = commit["result"]["path"].as_str().unwrap().to_string();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":43,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"这是什么"}},{{"type":"uploaded_image","uploadId":"{upload_id}"}}]}}}}"#
    ))
    .await;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    let seen = messages.lock().unwrap().clone();
    // 首轮收尾会为会话标题再调一次 provider（见 `TITLE_TIMEOUT`），故按**本轮**
    // 的用户文本挑出那次调用，而不是盲目取最后一次。
    let first = seen
        .iter()
        .find(|msgs| {
            msgs.iter().any(|m| {
                m.role == yi_agent_core::Role::User
                    && m.content.iter().any(
                        |b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t == "这是什么"),
                    )
            })
        })
        .expect("the provider must have been called with the turn");
    let first_user = first
        .iter()
        .find(|m| m.role == yi_agent_core::Role::User)
        .expect("the turn call must carry a user message");
    let image_path = first_user
        .content
        .iter()
        .find_map(|b| match b {
            yi_agent_core::ContentBlock::Image { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("the user message must carry an image content block")
        .expect("the image block must carry the stored relative path");
    assert_eq!(image_path, rel);
    assert!(
        workdir.path().join(&image_path).is_file(),
        "the block path must point at the stored copy: {image_path}"
    );
    let text = first_user
        .content
        .iter()
        .find_map(|b| match b {
            yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .expect("the text block must still be there");
    assert_eq!(text, "这是什么");
    h.shutdown().await;
}

/// 未知/已 abort/已过期的 `uploadId` 不再毒死整轮：那张图被**丢弃**，turn 照常
/// 起、文本照常送进模型，开启项上不带 `images`。用户于是能把消息发出去，而不是
/// 被一张找不回来的图永久卡住（chip 会留着，重试永远同样失败）。
///
/// 判据是「失败能不能靠重试自愈」：`uploadId` 未知意味着服务端手里已经没有重试
/// 所需的任何东西（记录被 TTL 收走或从未存在），重发必然复现同一结果 → 丢弃。
/// 与之相对，显式 `{type:"image", path}` 指不到文件是**请求本身**的问题，仍硬拒
/// （见 `turn_start_rejects_a_non_image_image_input`）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_drops_an_unknown_upload_id_and_still_runs_the_turn() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();

    let messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let messages_factory = Arc::clone(&messages);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CapturingMessagesProvider {
            messages: Arc::clone(&messages_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            mcp: None,
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":50,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"uploaded_image","uploadId":"upload-does-not-exist"}},{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 关键逆转：这条 `turn/start` 必须**成功**（旧行为是回 `invalid_params` 并
    // 拒绝整轮）。开启项照常发出，只是没有 `images`。
    let mut resp: Option<serde_json::Value> = None;
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..30 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(50)) {
            resp = Some(v.clone());
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") && opener.is_none() {
            opener = Some(v["params"]["item"].clone());
        }
        if resp.is_some() && opener.is_some() {
            break;
        }
    }
    let resp = resp.expect("turn/start must answer");
    assert!(
        resp.get("error").is_none(),
        "an unrecoverable upload must not fail the whole turn: {resp}"
    );
    assert!(
        resp["result"]["turn_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "the turn must really start: {resp}"
    );
    let item = opener.expect("the turn must still open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["text"], "hi", "the text must survive: {item}");
    assert!(
        item["images"].as_array().is_none_or(|a| a.is_empty()),
        "the dropped image must not be echoed: {item}"
    );

    // 这一轮必须真的跑到底，且状态回到 idle——「占了槽却起不来」的僵尸 turn
    // 只会停在 running（只有真正收尾的 turn 才会让主循环清掉活跃标记）。
    let mut completed = false;
    let mut idle = false;
    for _ in 0..80 {
        let v = h.read_value().await;
        match v.get("method").and_then(|m| m.as_str()) {
            Some("turn/completed") => completed = true,
            Some("thread/status/updated")
                if v["params"]["thread_id"] == serde_json::json!(tid)
                    && v["params"]["status"] == "idle" =>
            {
                idle = true
            }
            _ => {}
        }
        if completed && idle {
            break;
        }
    }
    assert!(
        completed,
        "the turn must run to completion after the image is dropped"
    );
    assert!(
        idle,
        "the thread must go back to idle, not stay a zombie turn"
    );

    // 文本真的到了模型：拿到 provider 的 user 消息，其中不得有任何图片块
    // （被丢弃的图既不该进 item，也不该进模型上下文）。
    let seen = messages.lock().unwrap().clone();
    // 首轮收尾会为会话标题再调一次 provider（见 `TITLE_TIMEOUT`），故按**本轮**
    // 的用户文本挑出那次调用，而不是盲目取最后一次。
    let first = seen
        .iter()
        .find(|msgs| {
            msgs.iter().any(|m| {
                m.role == yi_agent_core::Role::User
                    && m.content
                        .iter()
                        .any(|b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t == "hi"))
            })
        })
        .expect("the provider must have been called with the turn");
    let first_user = first
        .iter()
        .find(|m| m.role == yi_agent_core::Role::User)
        .expect("the turn call must carry a user message");

    assert!(
        !first_user
            .content
            .iter()
            .any(|b| matches!(b, yi_agent_core::ContentBlock::Image { .. })),
        "the dropped image must not reach the model: {first_user:?}"
    );
    assert!(
        first_user
            .content
            .iter()
            .any(|b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t == "hi")),
        "the text must reach the model: {first_user:?}"
    );
    h.shutdown().await;
}

/// 跨会话守卫:为 thread A 上传并 commit 的图,**不能**凭一个 `uploadId` 被摄进
/// thread B。`uploadId` 只是不透明的句柄,若反查只认 id 不认归属,任何知道/猜到
/// 该 id 的客户端就能把别人的会话里的图塞进自己的会话(跨会话取数)。
#[tokio::test(flavor = "multi_thread")]
async fn an_uploaded_image_cannot_be_ingested_into_another_thread() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid_a = start_thread(&mut h).await;
    let tid_b = start_thread(&mut h).await;

    let bytes = write_a_re_encodable_png_into_memory();
    let total = bytes.len() as u64;
    let begin = rpc(
        &mut h,
        20,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid_a, "name": "shot.png", "mime": "image/png", "size": total
        }),
    )
    .await;
    let upload_id = begin["result"]["uploadId"]
        .as_str()
        .expect("begin must return an uploadId")
        .to_string();
    let step = bytes.len().div_ceil(3);
    for (i, part) in bytes.chunks(step).enumerate() {
        let data = base64::engine::general_purpose::STANDARD.encode(part);
        let resp = rpc(
            &mut h,
            21 + i as u64,
            "image/upload/chunk",
            serde_json::json!({ "uploadId": upload_id, "index": i, "data": data }),
        )
        .await;
        assert!(resp.get("error").is_none(), "chunk {i}: {resp}");
    }
    let commit = rpc(
        &mut h,
        30,
        "image/upload/commit",
        serde_json::json!({ "uploadId": upload_id }),
    )
    .await;
    assert!(commit.get("error").is_none(), "{commit}");

    // thread B 用 A 的 uploadId 起 turn:归属不符与「未知」同等对待——那张图被
    // **丢弃**，turn 照常起（不再毒死整轮），开启项上不带 `images`。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":31,"method":"turn/start","params":{{"threadId":"{tid_b}","input":[{{"type":"uploaded_image","uploadId":"{upload_id}"}},{{"type":"text","text":"steal"}}]}}}}"#
    ))
    .await;
    let mut resp: Option<serde_json::Value> = None;
    let mut opener_b: Option<serde_json::Value> = None;
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(31)) {
            resp = Some(v.clone());
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") && opener_b.is_none() {
            opener_b = Some(v["params"]["item"].clone());
        }
        if resp.is_some() && opener_b.is_some() {
            break;
        }
    }
    let resp = resp.expect("turn/start must answer");
    assert!(
        resp.get("error").is_none(),
        "B must still run: the foreign image is dropped, not fatal: {resp}"
    );
    let item_b = opener_b.expect("thread B must open with an item");
    assert_eq!(item_b["text"], "steal", "{item_b}");
    assert!(
        item_b["images"].as_array().is_none_or(|a| a.is_empty()),
        "thread B must not receive thread A's image: {item_b}"
    );
    // 安全断言不变:A 的字节一个都没进 B 的工作区。
    assert!(
        !workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid_b)
            .exists(),
        "thread B must not receive thread A's image"
    );
    // 等 B 这轮收尾,再看 A 的正控——否则 B 的开启项会被下面的循环误当成 A 的。
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    // 正控:A 自己仍能正常摄取这张图。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":32,"method":"turn/start","params":{{"threadId":"{tid_a}","input":[{{"type":"uploaded_image","uploadId":"{upload_id}"}},{{"type":"text","text":"mine"}}]}}}}"#
    ))
    .await;
    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("thread A must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(
        item["images"][0]["path"]
            .as_str()
            .map(|p| p.starts_with(&format!(".yi-agent/attachments/{tid_a}/"))),
        Some(true),
        "thread A must ingest its own upload: {item}"
    );
    h.shutdown().await;
}

/// 上传的边界:乱序块、越界块、超 20 MiB 的声明都要被拒(错误响应,不 panic)。
#[tokio::test(flavor = "multi_thread")]
async fn image_upload_refuses_out_of_order_oversized_and_too_large_uploads() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 乱序:第一条就发 index 1。
    let begin = rpc(
        &mut h,
        60,
        "image/upload/begin",
        serde_json::json!({ "threadId": tid, "name": "x.png", "size": 6 }),
    )
    .await;
    let id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    let out_of_order = rpc(
        &mut h,
        61,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": id, "index": 1,
            "data": base64::engine::general_purpose::STANDARD.encode(b"def"),
        }),
    )
    .await;
    assert!(
        out_of_order["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("out-of-order"),
        "{out_of_order}"
    );

    // 越界:声明 4 字节却发 5 字节。
    let small = rpc(
        &mut h,
        62,
        "image/upload/begin",
        serde_json::json!({ "threadId": tid, "name": "y.png", "size": 4 }),
    )
    .await;
    let small_id = small["result"]["uploadId"].as_str().unwrap().to_string();
    let overrun = rpc(
        &mut h,
        63,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": small_id, "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(b"abcde"),
        }),
    )
    .await;
    assert!(
        overrun["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("overruns"),
        "{overrun}"
    );

    // 声明就超 20 MiB:begin 即拒。
    let too_big = rpc(
        &mut h,
        64,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid, "name": "huge.png",
            "size": yi_agent_tools::image_prep::MAX_IMAGE_BYTES + 1,
        }),
    )
    .await;
    assert!(
        too_big["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("over"),
        "{too_big}"
    );

    // 未知会话:chunk 与 commit 都拒绝,不 panic。
    let unknown_chunk = rpc(
        &mut h,
        65,
        "image/upload/chunk",
        serde_json::json!({ "uploadId": "upload-nope", "index": 0, "data": "" }),
    )
    .await;
    assert!(unknown_chunk.get("error").is_some(), "{unknown_chunk}");
    let unknown_commit = rpc(
        &mut h,
        66,
        "image/upload/commit",
        serde_json::json!({ "uploadId": "upload-nope" }),
    )
    .await;
    assert!(unknown_commit.get("error").is_some(), "{unknown_commit}");

    h.shutdown().await;
}

/// `abort` 清 staging:`image/upload/abort` 之后该 `uploadId` 再也无法 commit
/// 或用于 `turn/start`。
#[tokio::test(flavor = "multi_thread")]
async fn image_upload_abort_discards_the_staging_file() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let begin = rpc(
        &mut h,
        70,
        "image/upload/begin",
        serde_json::json!({ "threadId": tid, "name": "z.png", "size": 3 }),
    )
    .await;
    let id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    rpc(
        &mut h,
        71,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": id, "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(b"abc"),
        }),
    )
    .await;
    assert!(
        workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid)
            .join(".tmp")
            .join(&id)
            .is_file(),
        "the staging file must exist before abort"
    );

    let abort = rpc(
        &mut h,
        72,
        "image/upload/abort",
        serde_json::json!({ "uploadId": id }),
    )
    .await;
    assert!(abort.get("error").is_none(), "{abort}");
    assert!(
        !workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid)
            .join(".tmp")
            .join(&id)
            .exists(),
        "abort must remove the staging file"
    );
    let commit = rpc(
        &mut h,
        73,
        "image/upload/commit",
        serde_json::json!({ "uploadId": id }),
    )
    .await;
    assert!(commit.get("error").is_some(), "{commit}");
    h.shutdown().await;
}

/// `thread/delete` 必须连带prune 该 thread 的上传登记。否则一条 `Receiving`
/// 记录会带着它的 staging 槽一直活到 10 分钟 TTL,进程级会话名额被死 thread
/// 占着;该 `uploadId` 也仍被当成「这个 thread 的未完成上传」而非「未知」。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_thread_prunes_its_image_upload_sessions() {
    use base64::Engine as _;
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一条收了一半的会话(有 staging 文件、未 commit)……
    let begin = rpc(
        &mut h,
        80,
        "image/upload/begin",
        serde_json::json!({ "threadId": tid, "name": "half.png", "size": 6 }),
    )
    .await;
    let half_id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    rpc(
        &mut h,
        81,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": half_id, "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(b"abc"),
        }),
    )
    .await;
    // ……和一条已 commit 的会话(记录仍在,`turn/start` 靠它反查)。
    let bytes = write_a_re_encodable_png_into_memory();
    let begin = rpc(
        &mut h,
        82,
        "image/upload/begin",
        serde_json::json!({
            "threadId": tid, "name": "shot.png", "mime": "image/png", "size": bytes.len(),
        }),
    )
    .await;
    let done_id = begin["result"]["uploadId"].as_str().unwrap().to_string();
    rpc(
        &mut h,
        83,
        "image/upload/chunk",
        serde_json::json!({
            "uploadId": done_id, "index": 0,
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }),
    )
    .await;
    let commit = rpc(
        &mut h,
        84,
        "image/upload/commit",
        serde_json::json!({ "uploadId": done_id }),
    )
    .await;
    assert!(commit.get("error").is_none(), "{commit}");

    let staging = |id: &str| {
        workdir
            .path()
            .join(".yi-agent/attachments")
            .join(&tid)
            .join(".tmp")
            .join(id)
    };
    assert!(
        staging(&half_id).is_file(),
        "staging must exist before delete"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":90,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(90)) {
            assert!(v.get("error").is_none(), "delete must succeed: {v}");
            break;
        }
    }

    // 收了一半的会话必须已被 prune:此刻该 id 是**未知**上传(记录已随 thread
    // 一起消失),而不是「这条 thread 的未完成上传」——后者说明记录还活着。
    let after = rpc(
        &mut h,
        91,
        "image/upload/commit",
        serde_json::json!({ "uploadId": half_id }),
    )
    .await;
    let err = after["error"]["message"].as_str().unwrap_or_default();
    assert!(
        err.contains("upload"),
        "the deleted thread's upload must be unknown, got: {after}"
    );
    assert!(
        !staging(&half_id).exists(),
        "staging must be gone after delete"
    );
    h.shutdown().await;
}

/// 反向断言：图片必须真的以内容块送进**模型**（而不只是回显在 item 上）。
///
/// 沿用 `turn_start_prompt_reaching_the_model_contains_the_attachment_manifest`
/// 的抓取手法，但捕获的是 provider 收到的 **messages**：首条 user 消息的
/// content 里必须有一个带 `path` 的 `ContentBlock::Image`，文本块仍然存在。
#[tokio::test(flavor = "multi_thread")]
async fn an_image_reaches_the_model_as_a_content_block() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();

    let messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let messages_factory = Arc::clone(&messages);
    let build = move |session: Option<yi_agent_core::Session>,
                      _cwd: &std::path::Path,
                      _mode: crate::thread_store::ThreadMode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CapturingMessagesProvider {
            messages: Arc::clone(&messages_factory),
        });
        let config = yi_agent_core::AgentConfig::default();
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            mcp: None,
        })
    };
    let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("shot.png");
    let img = ::image::RgbImage::from_fn(8, 8, |_, _| ::image::Rgb([9, 9, 9]));
    img.save_with_format(&src, ::image::ImageFormat::Png)
        .unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"这是什么"}},{{"type":"image","path":"{}"}}]}}}}"#,
        src.display()
    ))
    .await;

    // 等到 turn 收尾，确保 provider 至少被调用过一次。
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    let seen = messages.lock().unwrap().clone();
    // 首轮收尾会为会话标题再调一次 provider（见 `TITLE_TIMEOUT`），故按**本轮**
    // 的用户文本挑出那次调用，而不是盲目取最后一次。
    let first = seen
        .iter()
        .find(|msgs| {
            msgs.iter().any(|m| {
                m.role == yi_agent_core::Role::User
                    && m.content.iter().any(
                        |b| matches!(b, yi_agent_core::ContentBlock::Text(t) if t == "这是什么"),
                    )
            })
        })
        .expect("the provider must have been called with the turn");
    let first_user = first
        .iter()
        .find(|m| m.role == yi_agent_core::Role::User)
        .expect("the turn call must carry a user message");
    let image = first_user
        .content
        .iter()
        .find_map(|b| match b {
            yi_agent_core::ContentBlock::Image { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("the user message must carry an image content block");
    let rel = image.expect("the image block must carry the stored relative path");
    assert!(rel.starts_with(".yi-agent/attachments/"), "{rel}");
    assert!(
        std::path::Path::new(&workdir.path().join(&rel)).is_file(),
        "the block path must point at the stored copy: {rel}"
    );
    let text = first_user
        .content
        .iter()
        .find_map(|b| match b {
            yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .expect("the text block must still be there");
    assert_eq!(text, "这是什么");
    assert!(
        !text.contains("Attached files"),
        "images are content blocks, not manifest entries: {text:?}"
    );

    h.shutdown().await;
}

/// `image/read` 必须把文件的**原始字节**分块回传：客户端按 `nextOffset`
/// 循环拉取、base64 解码后拼接，结果必须与磁盘上的文件逐字节相同，
/// 且最后一块的 `nextOffset` 为 `null`。
///
/// 这条测试同时钉住「`size` 是文件字节长度」与「`mediaType` 由猜测格式得出」
/// 两项契约（Task 5 的交接：`size` 必须与取回的字节数一致）。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_round_trips_a_file_in_chunks() {
    use base64::Engine as _;

    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let rel = format!(".yi-agent/attachments/{tid}/deadbeef-big.png");
    let stored = workdir.path().join(&rel);
    std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
    let source = write_an_uncompressed_png(&stored);
    assert!(
        source.len() > 512 * 1024,
        "the fixture must span several chunks, got {} bytes",
        source.len()
    );

    let mut offset: u64 = 0;
    let mut assembled: Vec<u8> = Vec::new();
    let mut media_type: Option<String> = None;
    let mut chunks = 0usize;
    for step in 0..64 {
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}","offset":{offset}}}}}"#,
            100 + step
        ))
        .await;
        let resp = h.read_value().await;
        assert!(resp.get("error").is_none(), "image/read failed: {resp}");
        let result = &resp["result"];
        assert_eq!(
            result["size"].as_u64(),
            Some(source.len() as u64),
            "size must be the file's byte length: {resp}"
        );
        assert_eq!(
            result["mediaType"].as_str(),
            Some("image/png"),
            "mediaType must come from the guessed format: {resp}"
        );
        media_type = result["mediaType"].as_str().map(str::to_string);
        let chunk = base64::engine::general_purpose::STANDARD
            .decode(result["data"].as_str().expect("data must be a string"))
            .expect("data must be valid base64");
        assert!(
            chunk.len() <= 512 * 1024,
            "a chunk must stay within IMAGE_READ_CHUNK_BYTES: {}",
            chunk.len()
        );
        assembled.extend_from_slice(&chunk);
        chunks += 1;
        match result["nextOffset"].as_u64() {
            Some(next) => {
                assert_eq!(next, offset + chunk.len() as u64, "nextOffset must advance");
                offset = next;
            }
            None => break,
        }
    }
    let expected_chunks = (source.len() as u64).div_ceil(512 * 1024) as usize;
    assert!(
        chunks == expected_chunks,
        "expected {expected_chunks} chunks of at most 512 KiB, took {chunks}"
    );
    assert!(chunks > 1, "the fixture must actually span several chunks");
    assert_eq!(media_type.as_deref(), Some("image/png"));
    assert_eq!(
        assembled.len(),
        source.len(),
        "the reassembled bytes must cover the whole file"
    );
    assert!(
        assembled == source,
        "the reassembled bytes must be byte-identical to the file on disk"
    );
}

/// 分片边界：`offset` 落在文件末尾之后按空块收尾（`nextOffset: null`），
/// 不得越界 panic；末块确实返回 `null` 而不是再一次偏移。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_reports_the_final_chunk_with_a_null_offset() {
    use base64::Engine as _;

    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let rel = format!(".yi-agent/attachments/{tid}/beef-small.png");
    let stored = workdir.path().join(&rel);
    std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
    let img = ::image::RgbImage::from_fn(8, 8, |_, _| ::image::Rgb([3, 4, 5]));
    img.save_with_format(&stored, ::image::ImageFormat::Png)
        .unwrap();
    let source = std::fs::read(&stored).unwrap();

    // 一次就取完：小图远小于一个分片。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":200,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert!(resp.get("error").is_none(), "image/read failed: {resp}");
    let result = &resp["result"];
    assert!(
        result["nextOffset"].is_null(),
        "the final chunk must report a null nextOffset: {resp}"
    );
    let chunk = base64::engine::general_purpose::STANDARD
        .decode(result["data"].as_str().unwrap())
        .unwrap();
    assert!(chunk == source, "the single chunk must be the whole file");

    // 越过末尾的 offset 只能得到空块，不得读越界或 panic。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":201,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}","offset":100000}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert!(resp.get("error").is_none(), "past-the-end read: {resp}");
    assert_eq!(resp["result"]["data"].as_str(), Some(""), "{resp}");
    assert!(resp["result"]["nextOffset"].is_null(), "{resp}");

    // `maxBytes` 生效：一次只要 4 字节，`nextOffset` 必须推进 4。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":202,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}","maxBytes":4}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert!(resp.get("error").is_none(), "sized read: {resp}");
    let first = base64::engine::general_purpose::STANDARD
        .decode(resp["result"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(first.len(), 4, "{resp}");
    assert_eq!(resp["result"]["nextOffset"].as_u64(), Some(4), "{resp}");

    // `maxBytes: 0` 不得造成 `nextOffset` 不前进（否则客户端死循环）：按未
    // 指定处理，整张图一次取完。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":203,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}","maxBytes":0}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert!(resp.get("error").is_none(), "zero-sized read: {resp}");
    let whole = base64::engine::general_purpose::STANDARD
        .decode(resp["result"]["data"].as_str().unwrap())
        .unwrap();
    assert!(
        whole == source,
        "maxBytes:0 must fall back to the chunk cap"
    );
    h.shutdown().await;
}

/// 限根是安全边界：`../` 逃逸与工作区外的绝对路径都必须以 `invalid_params`
/// 拒绝——`image/read` 不得成为任意文件读取原语。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_a_path_outside_the_workspace() {
    let tmp = tempfile::TempDir::new().unwrap();
    // cwd 是子目录，父目录里放一张**真图**：这样「被拒」只能是因为越界，
    // 而不是因为文件不存在或不是图片。
    let cwd = tmp.path().join("ws");
    std::fs::create_dir_all(&cwd).unwrap();
    let secret = tmp.path().join("secret.png");
    let img = ::image::RgbImage::from_fn(8, 8, |_, _| ::image::Rgb([1, 1, 1]));
    img.save_with_format(&secret, ::image::ImageFormat::Png)
        .unwrap();

    let mut cfg = default_config();
    cfg.workdir = cwd.clone();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // (a) 相对路径向上逃逸。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":300,"method":"image/read","params":{{"threadId":"{tid}","path":"../secret.png"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "a relative escape must be refused as invalid_params: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("path escapes the workspace"),
        "the escape must be caught by the confinement check, not by a fallback: {resp}"
    );

    // (b) 工作区外的绝对路径（`Path::join` 遇绝对路径会整体替换）。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":301,"method":"image/read","params":{{"threadId":"{tid}","path":"{}"}}}}"#,
        secret.display()
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "an outside absolute path must be refused as invalid_params: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("path escapes the workspace"),
        "the absolute path must be caught by the confinement check: {resp}"
    );
    h.shutdown().await;
}

/// 只送可解码的图片：一个文本文件即使在工作区内也必须被拒。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_a_non_image_file() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    std::fs::write(workdir.path().join("notes.txt"), b"just some text\n").unwrap();
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":400,"method":"image/read","params":{{"threadId":"{tid}","path":"notes.txt"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "a non-image must be refused as invalid_params: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("not a supported image"),
        "the text file must be rejected by the format guess: {resp}"
    );

    // 把 PDF 伪装成 .png 的扩展名也一样：判定看的是字节，不是名字。
    std::fs::write(workdir.path().join("fake.png"), b"%PDF-1.4 fake\n").unwrap();
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":401,"method":"image/read","params":{{"threadId":"{tid}","path":"fake.png"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "the extension must not decide; the bytes must: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("not a supported image"),
        "the mislabelled PDF must be rejected by the format guess: {resp}"
    );
    h.shutdown().await;
}

/// 超过 `IMAGE_READ_MAX_BYTES`（20 MiB）的图必须被拒——分片只是传输形态，
/// 不代表可以无上限地服务任意大文件。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_an_oversized_image() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一个宽 6000px 的 PNG 位图够 20 MiB；重新编码成 PNG 更容易超限。
    let rel = format!(".yi-agent/attachments/{tid}/huge.png");
    let stored = workdir.path().join(&rel);
    std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
    write_an_uncompressed_png(&stored);
    // 直接从磁盘读回来按字节补齐到 20 MiB 以上；文件本身仍是同一张真 PNG
    // （PNG 允许附加尾随数据，`guess_format` 只看魔数，尺寸检查据此拒绝）。
    let mut bytes = std::fs::read(&stored).unwrap();
    bytes.resize(20 * 1024 * 1024 + 1, 0);
    std::fs::write(&stored, &bytes).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":500,"method":"image/read","params":{{"threadId":"{tid}","path":"{rel}"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "an image past IMAGE_READ_MAX_BYTES must be refused: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("image too large"),
        "the refusal must come from the size gate: {resp}"
    );

    // 一个超过上限的**非图片**同样要在读取/解码之前被拒：闸门按元数据长度
    // 先设，故它报的是尺寸而不是「不是图片」——证明没有先把整个文件读进来。
    let huge_text = workdir.path().join("huge.txt");
    std::fs::write(&huge_text, vec![b'a'; 20 * 1024 * 1024 + 1]).unwrap();
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":501,"method":"image/read","params":{{"threadId":"{tid}","path":"huge.txt"}}}}"#
    ))
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "an oversized file of any kind must be refused: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("image too large"),
        "the metadata gate must fire before the format guess: {resp}"
    );
    h.shutdown().await;
}

/// 非普通文件必须被拒：`thread` 的 cwd 下可达的一个 FIFO（agent 或克隆下来的
/// 仓库都可能放进来）元数据长度为 `0`，能通过尺寸闸；而 `std::fs::read` 的
/// `open()` 会在无写端时**阻塞**，把整条 serve 循环（所有客户端/thread）挂住。
/// 本 RPC 是安全边界，故 `!is_file()` 必须直接拒绝，绝不进入读取。
///
/// 此处**自建 runtime** 而非用 `#[tokio::test]`：修复回退时那个 worker 会卡在
/// `open()` 上无法 join，若由 `#[tokio::test]` 代管，进程在 teardown 时挂住、
/// 整个套件被拖死。自建 runtime 让测试用 `tokio::time::timeout` 把它变成一次
/// **可读的失败**，再 `mem::forget` 掉这个 join 不掉的 runtime，进程照常退出。
#[test]
fn image_read_refuses_a_non_regular_file() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let mut h = rt.block_on(Harness::with_cfg(cfg));
    let tid = rt.block_on(start_thread(&mut h));

    let fifo = workdir
        .path()
        .join(".yi-agent/attachments")
        .join(&tid)
        .join("trap.png");
    std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
    // 经 `mkfifo` 造 FIFO——`libc` 不是本 crate 的依赖，不为一条测试新增它。
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo must be available to exercise the FIFO path");
    assert!(status.success(), "mkfifo failed for {fifo:?}");
    let md = std::fs::symlink_metadata(&fifo).unwrap();
    assert!(
        !md.is_file() && !md.is_dir(),
        "the fixture must really be a non-regular file: {fifo:?}"
    );

    rt.block_on(h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":550,"method":"image/read","params":{{"threadId":"{tid}","path":".yi-agent/attachments/{tid}/trap.png"}}}}"#
    )));
    // 修复缺失时 `image/read` 的 `fs::read` 会卡在 `open()` 上：此处**自己**
    // 给 `read_line` 设 5s 上限（而不是用 `Harness::read_value`，它内部的
    // 5s 超时会抢先 panic 掉），超时即说明它**没有**拒绝。超时后把这个卡住的
    // `read_line` future 直接丢掉，再泄露 runtime、再断言失败——给出一次
    // 干净的 RED 输出，而不是让整个测试套件挂死。
    let mut buf = String::new();
    let got = rt
        .block_on(async { tokio::time::timeout(EVENT_WAIT, h.client_r.read_line(&mut buf)).await });
    let Ok(Ok(n)) = got else {
        std::mem::forget(rt);
        panic!("image/read must not block on a non-regular file");
    };
    assert!(n > 0, "unexpected EOF while waiting for the refusal");
    let resp: serde_json::Value =
        serde_json::from_str(buf.trim()).expect("server wrote invalid JSON");
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "a non-regular file must be refused as invalid_params: {resp}"
    );
    assert_eq!(
        resp["error"]["message"].as_str(),
        Some("not a regular file"),
        "the refusal must come from the regular-file check: {resp}"
    );
    rt.block_on(h.shutdown());
}

/// 未知 thread 必须拒绝（`thread_cwd` 两条解析路径都落空）。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_an_unknown_thread() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":600,"method":"image/read","params":{"threadId":"thread-nope","path":"a.png"}}"#,
    )
    .await;
    let resp = h.read_value().await;
    assert_eq!(
        resp["error"]["code"].as_i64(),
        Some(-32602),
        "an unknown thread must be refused: {resp}"
    );
    h.shutdown().await;
}

/// 只发附件、不打字也必须能起 turn（空 input 校验放宽到「既无文本又无附件」）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_accepts_an_attachment_only_message() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("only.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut resp: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(9)) {
            resp = Some(v);
            break;
        }
    }
    let resp = resp.expect("turn/start must answer");
    assert!(
        resp.get("error").is_none(),
        "attachment-only start must succeed: {resp:?}"
    );
    assert!(
        resp["result"]["turn_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "an attachment-only turn must still return a turn_id: {resp:?}"
    );
    h.shutdown().await;
}

/// 只发图片、不打字也必须能起 turn——空 input 校验放宽到「既无文本、又无附件、
/// 又无图片」三缺一才算空（控制器裁定 2）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_accepts_an_image_only_message() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("only.png");
    let img = ::image::RgbImage::from_fn(8, 8, |_, _| ::image::Rgb([7, 7, 7]));
    img.save_with_format(&src, ::image::ImageFormat::Png)
        .unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut resp: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(9)) {
            resp = Some(v);
            break;
        }
    }
    let resp = resp.expect("turn/start must answer");
    assert!(
        resp.get("error").is_none(),
        "image-only start must succeed: {resp:?}"
    );
    assert!(
        resp["result"]["turn_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "an image-only turn must still return a turn_id: {resp:?}"
    );
    h.shutdown().await;
}

/// 不是图片的「图片输入」（例如把 PDF 塞进 `type:"image"`）必须就地拒绝：
/// 错误提到 image，且**一个 turn 都不起**（准备失败早于占位与发帧）。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_rejects_a_non_image_image_input() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("not-an-image.png");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":11,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}},{{"type":"text","text":"hi"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut methods_before_response: Vec<String> = Vec::new();
    let mut resp: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(11)) {
            resp = Some(v);
            break;
        }
        if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
            methods_before_response.push(m.to_string());
        }
    }
    let resp = resp.expect("turn/start must answer");
    let err = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(err.contains("image"), "got: {resp}");
    assert!(
        methods_before_response.is_empty(),
        "no turn may start on a bad image: {methods_before_response:?}"
    );
    // 而且活跃标记必须干净：同一 thread 的**下一轮**仍能正常起（不是
    // 「已占用但起不来」的僵尸 turn）。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":12,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    let mut second: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(12)) {
            second = Some(v);
            break;
        }
    }
    let second = second.expect("the follow-up turn/start must answer");
    assert!(
        second.get("error").is_none(),
        "a rejected image must not leave the turn slot occupied: {second:?}"
    );
    h.shutdown().await;
}

/// 显式路径的图片被拒时，错误必须带一个**结构化**判别码（`data.code = "invalid_image"`），
/// 客户端才不必去猜/子串匹配那条人话 message。数字码仍是 `invalid_params`——这次
/// 拒绝确实是"参数不合法"，加码只是让调用方能分级处理。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_reports_a_structured_code_for_a_bad_image_path() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("missing.png"); // 不创建：路径指不到任何文件

    let resp = rpc(
        &mut h,
        13,
        "turn/start",
        serde_json::json!({
            "threadId": tid,
            "input": [
                { "type": "image", "path": src.display().to_string() },
                { "type": "text", "text": "hi" }
            ],
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"].as_i64(), Some(-32602), "{resp}");
    assert_eq!(resp["error"]["data"]["code"], "invalid_image", "{resp}");
    // 人话里仍带 image，供不看码的客户端兜底。
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("image"),
        "{resp}"
    );
    h.shutdown().await;
}

/// 显式路径的图片被拒**不改变**声明大小以外的判据：非图内容（PDF 冒充 .png）也
/// 走同一条 `invalid_image`，报错的仍是路径那一侧。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_reports_a_structured_code_for_a_non_image_file() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("fake.png");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    let resp = rpc(
        &mut h,
        14,
        "turn/start",
        serde_json::json!({
            "threadId": tid,
            "input": [
                { "type": "image", "path": src.display().to_string() },
                { "type": "text", "text": "hi" }
            ],
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"].as_i64(), Some(-32602), "{resp}");
    assert_eq!(resp["error"]["data"]["code"], "invalid_image", "{resp}");
    h.shutdown().await;
}

/// 读不到的附件路径必须就地拒绝（错误提到 attachment），且一个 turn 都不起。
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_rejects_an_unreadable_attachment() {
    let mut h = Harness::new();
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":10,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"/definitely/not/here.pdf"}},{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;

    // 应答之前不得有任何通知:准备失败要发生在发帧之前。
    let mut methods_before_response: Vec<String> = Vec::new();
    let mut resp: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(10)) {
            resp = Some(v);
            break;
        }
        if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
            methods_before_response.push(m.to_string());
        }
    }
    let resp = resp.expect("turn/start must answer");
    let err = resp["error"]["message"].as_str().unwrap_or_default();
    assert!(err.contains("attachment"), "got: {resp}");
    assert!(
        methods_before_response.is_empty(),
        "no turn may start on a bad attachment: {methods_before_response:?}"
    );
    h.shutdown().await;
}

/// Task 7:`thread/delete` 要连带收走该会话的附件目录——附件与 thread 文件同一
/// 生命周期（设计 §6(d)）。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_thread_removes_its_attachments() {
    // 自带 workdir:默认目录跨次运行会残留同名附件,「盘上存在」的断言会被旧
    // 文件蒙混过关。
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    // 源文件放在工作区外,模拟「从 Downloads 选文件」。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("报告.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"hi"}}]}}}}"#,
        src.display()
    ))
    .await;
    // 起带附件的 turn,读到 `turn/completed` 说明准备期已过(附件在发帧之前
    // 复制)、且 turn 已结束,删除不会落在半途。
    let mut saw_response = false;
    let mut completed = false;
    while !(saw_response && completed) {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            assert!(v.get("error").is_none(), "turn/start must succeed: {v}");
            saw_response = true;
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = true;
        }
    }

    let att_dir = workdir.path().join(".yi-agent/attachments").join(&tid);
    assert!(
        att_dir.is_dir(),
        "attachment dir should exist before delete"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    loop {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(5)) {
            assert!(v.get("error").is_none(), "delete must succeed: {v}");
            break;
        }
    }

    assert!(
        !att_dir.exists(),
        "attachments must be reclaimed with the thread"
    );
    h.shutdown().await;
}

/// Task 7:`/clear` 只截断历史,会话还在(clear 之后照样能继续),附件不清——
/// 这是与 `thread/delete` 的关键区别(设计 §6(e))。
#[tokio::test(flavor = "multi_thread")]
async fn clearing_a_thread_keeps_its_attachments() {
    // 设计 §6(e):/clear 截断历史但会话还在,附件不清。
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("报告.pdf");
    std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"hi"}}]}}}}"#,
        src.display()
    ))
    .await;
    // 等 turn 跑完:`thread/clear` 在 turn 进行中会被拒(turn_in_progress)。
    let mut saw_response = false;
    let mut completed = false;
    while !(saw_response && completed) {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(3)) {
            assert!(v.get("error").is_none(), "turn/start must succeed: {v}");
            saw_response = true;
        }
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            completed = true;
        }
    }

    let att_dir = workdir.path().join(".yi-agent/attachments").join(&tid);
    assert!(att_dir.is_dir(), "attachment dir should exist before clear");

    // 主循环要等 `TurnEvent::Finished` 才清 `active_turn_id`,刚跑完一轮立刻
    // clear 可能撞上 -32012(turn 进行中)的收尾窗口——用现成的重试 helper,
    // 与其它 clear 测试同一口径。
    let resp = clear_thread_retrying(&mut h, &tid).await;
    assert!(resp.get("error").is_none(), "clear must succeed: {resp}");

    assert!(att_dir.is_dir(), "thread/clear must not remove attachments");
    h.shutdown().await;
}

/// 纯图片输入（`display_text` 为空）不生成标题：没有可喂给标题模型的文本。
///
/// 这是刻意的边界，spec §4 的错误处理表已列明。用 `assert_eq!(.., 0)` 而不是
/// 「标题为空」——后者在兜底标题恰好也没写入时会误通过。
#[tokio::test(flavor = "multi_thread")]
async fn image_only_input_does_not_generate_a_title() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let png = dir.path().join("shot.png");
    ::image::RgbImage::from_fn(8, 8, |_, _| ::image::Rgb([7, 7, 7]))
        .save_with_format(&png, ::image::ImageFormat::Png)
        .unwrap();

    let title_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CountingTitleProvider {
        title_calls: Arc::clone(&title_calls),
    });
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(provider.clone(), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}}]}}}}"#,
        png.display()
    ))
    .await;
    let _ = read_until_method(&mut h, "turn/completed").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        title_calls.load(Ordering::SeqCst),
        0,
        "an image-only message has no text to title from"
    );
    h.shutdown().await;
}

/// `image/read` 只应触碰被请求的那一段，而不是把整个文件读进内存再切片。
///
/// 判据用**相对比较**：同一个 20 MiB 文件，取最后一页 1 字节的耗时不应显著
/// 高于取首页 1 字节。全量读实现下两者都是「读满 20 MiB」，比值接近 1 但
/// **绝对开销与文件大小成正比**;区间读实现下两者都只碰 16 字节头 + 1 字节。
/// 故这里改用更锐利的判据：页内字节数相同,比较 20 MiB 与 1 KiB 两个文件。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_does_not_scale_with_file_size() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 两张「png」：魔数正确即可通过 guess_format，其余填充随机性无关。
    let png_magic: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let big_path = dir.path().join("big.png");
    let mut big = png_magic.to_vec();
    big.resize(20 * 1024 * 1024, 0x41);
    std::fs::write(&big_path, &big).unwrap();

    let small_path = dir.path().join("small.png");
    let mut small = png_magic.to_vec();
    small.resize(1024, 0x41);
    std::fs::write(&small_path, &small).unwrap();

    // 取各自**最后一页**的第一字节：两者读取量都是 1 字节。
    let big_offset = big.len() as u64 - 1;
    let small_offset = small.len() as u64 - 1;

    // 自由函数而不是闭包：闭包捕获 &mut h 后无法在循环里连续 await 两次。
    async fn time_one_read(
        h: &mut Harness,
        tid: &str,
        path: &str,
        offset: u64,
        id: u64,
    ) -> std::time::Duration {
        let started = std::time::Instant::now();
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"image/read","params":{{"threadId":"{tid}","path":"{path}","offset":{offset},"maxBytes":1}}}}"#
        ))
        .await;
        let v = h
            .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(id)))
            .await;
        assert_eq!(v["result"]["nextOffset"], serde_json::Value::Null, "{v}");
        started.elapsed()
    }

    // 各跑三次取最小值，压掉文件系统缓存冷启动的抖动。串行 await，
    // 不能用 join!/select!——那会交错两次读取，测不出单次开销。
    let mut big_best = std::time::Duration::MAX;
    let mut small_best = std::time::Duration::MAX;
    let mut id = 80u64;
    for _ in 0..3 {
        big_best = big_best.min(time_one_read(&mut h, &tid, "big.png", big_offset, id).await);
        id += 1;
        small_best =
            small_best.min(time_one_read(&mut h, &tid, "small.png", small_offset, id).await);
        id += 1;
    }
    assert!(
        big_best < small_best * 4,
        "one 1-byte read must not scale with file size: \
         20MiB={big_best:?} 1KiB={small_best:?}"
    );
    h.shutdown().await;
}
