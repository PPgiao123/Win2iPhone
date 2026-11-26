use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION, USER_AGENT},
    redirect::Policy,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    env,
    fs::{self, File},
    io::{self, Cursor},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;
use zip::ZipArchive;

const WORKFLOW_FILE_NAME: &str = "unity-ios-from-xcode.yml";
const WORKFLOW_YML: &str = include_str!("../../workflows/unity-ios-from-xcode.yml");
const BUILTIN_GITHUB_CLIENT_ID: Option<&str> = option_env!("UNITY_IOS_GITHUB_CLIENT_ID");
const DEFAULT_GITHUB_CLIENT_ID: &str = "Ov23liqKffZYnsbi99e8";

const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_OAUTH_APPS_URL: &str = "https://github.com/settings/developers";

pub struct GitHubAuth {
    pub login: String,
    pub token: Zeroizing<String>,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_poll_interval")]
    interval: u64,
}

fn default_poll_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct UserResponse {
    login: String,
}

#[derive(Deserialize)]
struct RepoResponse {
    private: bool,
    default_branch: String,
}

#[derive(Deserialize)]
struct ContentResponse {
    content: String,
    sha: String,
}

#[derive(Deserialize)]
struct ReleaseResponse {
    upload_url: String,
}

#[derive(Deserialize)]
struct DispatchResponse {
    workflow_run_id: Option<u64>,
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct RunsListResponse {
    workflow_runs: Vec<WorkflowRunSummary>,
}

#[derive(Deserialize)]
struct WorkflowRunSummary {
    id: u64,
    status: String,
    conclusion: Option<String>,
    html_url: String,
    head_branch: Option<String>,
}

#[derive(Deserialize)]
struct ArtifactsListResponse {
    artifacts: Vec<ArtifactSummary>,
}

#[derive(Deserialize)]
struct ArtifactSummary {
    name: String,
    archive_download_url: String,
    expired: bool,
}

pub fn build_http_client() -> Result<reqwest::Client> {
    let ua = format!("Unity-IPA-Installer/{}", env!("CARGO_PKG_VERSION"));
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_str(&ua)?);
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(20))
        .build()
        .context("无法初始化 HTTPS 客户端")
}

fn auth_dir() -> Result<PathBuf> {
    Ok(
        PathBuf::from(env::var_os("LOCALAPPDATA").context("无法定位 LOCALAPPDATA")?)
            .join("UnityIpaWorkflow")
            .join("Auth"),
    )
}

fn token_path() -> Result<PathBuf> {
    Ok(auth_dir()?.join("github-token.dpapi"))
}

fn client_id_path() -> Result<PathBuf> {
    Ok(auth_dir()?.join("github-client-id.txt"))
}

pub fn has_saved_token() -> bool {
    token_path().is_ok_and(|p| p.is_file())
}

fn load_saved_token() -> Option<Zeroizing<String>> {
    let path = token_path().ok()?;
    let encrypted = fs::read(path).ok()?;
    let bytes = crate::pairing::crypt(&encrypted, false).ok()?;
    let text = String::from_utf8(bytes.to_vec()).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| Zeroizing::new(trimmed.to_owned()))
}

fn save_token(token: &str) -> Result<()> {
    let path = token_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let encrypted = crate::pairing::crypt(token.as_bytes(), true)?;
    fs::write(path, encrypted)?;
    Ok(())
}

pub fn logout() -> Result<()> {
    let path = token_path()?;
    if path.is_file() {
        fs::remove_file(&path)?;
        println!("已清除本机保存的 GitHub 登录凭据。");
    } else {
        println!("本机未保存 GitHub 登录凭据。");
    }
    Ok(())
}

fn resolve_client_id() -> Result<String> {
    if let Ok(val) = env::var("UNITY_IOS_GITHUB_CLIENT_ID") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_owned());
        }
    }
    if let Some(val) = BUILTIN_GITHUB_CLIENT_ID.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(val.to_owned());
    }
    if let Ok(path) = client_id_path() {
        if let Ok(saved) = fs::read_to_string(&path) {
            let trimmed = saved.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_owned());
            }
        }
    }
    let default_id = DEFAULT_GITHUB_CLIENT_ID.trim();
    if !default_id.is_empty() {
        return Ok(default_id.to_owned());
    }

    println!();
    println!("首次使用未内置 OAuth Client ID 的开发版，需要填写一次 GitHub OAuth App Client ID：");
    println!("  1. 打开 {GITHUB_OAUTH_APPS_URL} → New OAuth App");
    println!("  2. Homepage/Callback 填 http://localhost 即可");
    println!("  3. 在创建页面勾选「Enable Device Flow」，复制 Client ID（无需 Client Secret）");
    if crate::prompt("按 Enter 自动打开 GitHub OAuth 设置页，或输入 s 直接粘贴 Client ID")?
        .eq_ignore_ascii_case("")
    {
        let _ = crate::winui::open_url(GITHUB_OAUTH_APPS_URL);
    }
    let id = crate::prompt("请粘贴 GitHub OAuth Client ID")?;
    ensure!(
        !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')),
        "GitHub Client ID 格式无效。"
    );
    let path = client_id_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &id)?;
    Ok(id)
}

/// Ensures we have a valid GitHub token with `repo` and `workflow` scopes, running OAuth Device Flow if needed.
pub async fn ensure_github_auth(http: &reqwest::Client) -> Result<GitHubAuth> {
    if let Some(saved) = load_saved_token() {
        match verify_token(http, &saved).await {
            Ok(Some(login)) => {
                println!("✅ 已登录 GitHub 账号：{login}");
                return Ok(GitHubAuth {
                    login,
                    token: saved,
                });
            }
            Ok(None) => {
                println!("本机 GitHub 登录已过期或权限不足，需要重新授权。");
            }
            Err(err) => {
                eprintln!("检查本机 GitHub 登录状态失败：{err:#}");
            }
        }
    }

    let client_id = resolve_client_id()?;
    println!("正在向 GitHub 请求设备登录授权码...");
    let code_resp: DeviceCodeResponse = http
        .post(GITHUB_DEVICE_CODE_URL)
        .header(ACCEPT, "application/json")
        .json(&json!({
            "client_id": client_id,
            "scope": "repo workflow"
        }))
        .send()
        .await
        .context("无法连接 GitHub 登录服务")?
        .error_for_status()
        .context("GitHub 设备授权初始化失败，请检查 OAuth Client ID 是否启用了 Device Flow")?
        .json()
        .await
        .context("无法解析 GitHub 设备授权响应")?;

    println!();
    println!("请在浏览器中完成 GitHub 授权：");
    println!("  网址：{}", code_resp.verification_uri);
    println!("  验证码：{}", code_resp.user_code);
    if let Err(err) = crate::winui::open_url(&code_resp.verification_uri) {
        eprintln!("{err:#}");
    } else {
        println!("已自动为您打开授权页面，输入上方验证码并确认即可。");
    }

    let mut poll_secs = code_resp.interval.max(5);
    let deadline = Instant::now() + Duration::from_secs(code_resp.expires_in.max(60));
    let token = loop {
        ensure!(Instant::now() < deadline, "GitHub 授权超时，请重试。");
        tokio::time::sleep(Duration::from_secs(poll_secs)).await;

        let token_resp: AccessTokenResponse = http
            .post(GITHUB_ACCESS_TOKEN_URL)
            .header(ACCEPT, "application/json")
            .json(&json!({
                "client_id": client_id,
                "device_code": code_resp.device_code,
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
            }))
            .send()
            .await
            .context("轮询 GitHub 授权状态失败")?
            .json()
            .await
            .context("解析 GitHub 令牌响应失败")?;

        if let Some(access_token) = token_resp.access_token {
            break Zeroizing::new(access_token);
        }
        match token_resp.error.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                poll_secs += 5;
                continue;
            }
            Some("access_denied") => bail!("您已取消 GitHub 授权。"),
            Some("expired_token") => bail!("GitHub 设备验证码已过期，请重试。"),
            Some(other) => {
                let desc = token_resp.error_description.as_deref().unwrap_or("");
                bail!("GitHub 授权失败（{other} {desc}）。");
            }
            None => bail!("GitHub 返回了无效的授权响应。"),
        }
    };

    let login = verify_token(http, &token)
        .await?
        .context("已取得令牌但权限验证失败，请确认授权页面勾选了 repo 与 workflow 权限。")?;
    if let Err(err) = save_token(&token) {
        eprintln!("警告：未能将 GitHub 登录状态保存到本机凭据文件：{err:#}");
    }
    println!("✅ GitHub 授权成功：{login}");
    Ok(GitHubAuth { login, token })
}

async fn verify_token(http: &reqwest::Client, token: &str) -> Result<Option<String>> {
    let resp = github_get(http, token, &format!("{GITHUB_API_BASE}/user"))
        .send()
        .await
        .context("无法连接 GitHub API")?;
    if resp.status() == StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    if !resp.status().is_success() {
        bail!("GitHub 用户信息查询失败（HTTP {}）。", resp.status());
    }
    let scopes = resp
        .headers()
        .get("x-oauth-scopes")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !has_required_scopes(scopes) {
        return Ok(None);
    }
    let user: UserResponse = resp.json().await.context("无法解析 GitHub 用户响应")?;
    Ok(Some(user.login))
}

pub fn has_required_scopes(scopes_header: &str) -> bool {
    let list: Vec<&str> = scopes_header
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    list.iter().any(|s| *s == "repo") && list.iter().any(|s| *s == "workflow")
}

/// Uploads the Xcode archive to a private GitHub repository, triggers the macOS Actions build, and downloads + verifies the unsigned IPA.
pub async fn run_cloud_ipa_build(
    http: &reqwest::Client,
    auth: &GitHubAuth,
    export: &crate::unity::ExportArtifact,
    repo_override: Option<&str>,
) -> Result<PathBuf> {
    let repo_slug = repo_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}/unity-ios-builds", auth.login));
    ensure!(
        validate_repo_slug(&repo_slug),
        "GitHub 仓库格式必须为 owner/repo。"
    );

    let confirm = crate::prompt(&format!(
        "将 Xcode 工程上传到 GitHub 私有仓库 {repo_slug} 并启动 macOS 云端编译。按 Enter 继续，输入其他字符取消"
    ))?;
    ensure!(confirm.is_empty(), "已取消云端编译。");

    let (owner, repo_name) = repo_slug.split_once('/').context("仓库格式无效")?;
    let repo_url = format!("{GITHUB_API_BASE}/repos/{repo_slug}");
    let repo_info: RepoResponse = match github_get(http, &auth.token, &repo_url).send().await? {
        resp if resp.status() == StatusCode::NOT_FOUND => {
            ensure!(
                owner.eq_ignore_ascii_case(&auth.login),
                "找不到目标仓库 {repo_slug}，或当前 GitHub 账号没有创建权限。"
            );
            println!("正在创建私有构建仓库 {repo_slug}...");
            github_post(http, &auth.token, &format!("{GITHUB_API_BASE}/user/repos"))
                .json(&json!({
                    "name": repo_name,
                    "private": true,
                    "auto_init": true
                }))
                .send()
                .await
                .context("创建 GitHub 私有仓库失败")?
                .error_for_status()
                .context("GitHub 拒绝创建私有仓库")?
                .json()
                .await
                .context("解析新建仓库响应失败")?
        }
        resp if resp.status().is_success() => resp.json().await?,
        resp => bail!("读取 GitHub 仓库 {repo_slug} 失败（HTTP {}）。", resp.status()),
    };
    ensure!(
        repo_info.private,
        "安全限制：只能使用私有仓库（Private Repository）存放 Xcode 工程导出物。"
    );

    sync_workflow_file(http, &auth.token, &repo_slug, &repo_info.default_branch).await?;

    println!("正在创建私有构建 Release 并上传 Xcode 归档...");
    let release_resp: ReleaseResponse = github_post(
        http,
        &auth.token,
        &format!("{GITHUB_API_BASE}/repos/{repo_slug}/releases"),
    )
    .json(&json!({
        "tag_name": export.tag,
        "target_commitish": repo_info.default_branch,
        "name": export.tag,
        "prerelease": true,
        "body": "Private Unity Xcode export for an iPhone test build."
    }))
    .send()
    .await
    .context("创建 GitHub Release 失败")?
    .error_for_status()
    .context("GitHub 拒绝创建 Release")?
    .json()
    .await
    .context("解析 GitHub Release 响应失败")?;

    let upload_base = strip_upload_url_template(&release_resp.upload_url);
    let upload_url = format!("{upload_base}?name=unity-ios-xcode.tar.gz");
    let archive_bytes = fs::read(&export.archive_path)
        .with_context(|| format!("无法读取归档 {}", export.archive_path.display()))?;
    println!(
        "正在上传 {:.1} MB Xcode 工程压缩包...",
        archive_bytes.len() as f64 / (1024.0 * 1024.0)
    );
    github_post(http, &auth.token, &upload_url)
        .header(CONTENT_TYPE, "application/gzip")
        .timeout(Duration::from_secs(600))
        .body(archive_bytes)
        .send()
        .await
        .context("上传 Xcode 归档到 GitHub 失败")?
        .error_for_status()
        .context("GitHub 拒绝接收 Xcode 归档附件")?;

    println!("正在触发 GitHub Actions macOS 云端编译...");
    let prev_ids = fetch_recent_run_ids(http, &auth.token, &repo_slug).await.unwrap_or_default();
    let dispatch_resp = github_post(
        http,
        &auth.token,
        &format!("{GITHUB_API_BASE}/repos/{repo_slug}/actions/workflows/{WORKFLOW_FILE_NAME}/dispatches"),
    )
    .header("X-GitHub-Api-Version", "2026-03-10")
    .json(&json!({
        "ref": repo_info.default_branch,
        "inputs": {
            "release_tag": export.tag,
            "archive_sha256": export.sha256
        }
    }))
    .send()
    .await
    .context("触发 GitHub Actions 工作流失败")?;
    ensure!(
        dispatch_resp.status().is_success(),
        "GitHub Actions 触发失败（HTTP {}）。请检查仓库 Actions 是否已启用。",
        dispatch_resp.status()
    );

    let mut run_id = None;
    let mut html_url = format!("https://github.com/{repo_slug}/actions");
    if dispatch_resp.status() == StatusCode::OK {
        if let Ok(parsed) = dispatch_resp.json::<DispatchResponse>().await {
            run_id = parsed.workflow_run_id;
            if let Some(url) = parsed.html_url {
                html_url = url;
            }
        }
    }

    let run_id = match run_id {
        Some(id) => id,
        None => {
            let mut found = None;
            for _ in 0..15 {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Ok(runs) = fetch_recent_runs(http, &auth.token, &repo_slug).await {
                    if let Some(run) = runs.into_iter().find(|r| {
                        !prev_ids.contains(&r.id)
                            && r.head_branch.as_deref() == Some(repo_info.default_branch.as_str())
                    }) {
                        html_url = run.html_url;
                        found = Some(run.id);
                        break;
                    }
                }
            }
            found.with_context(|| {
                format!("已触发构建但未能获取运行编号，请打开查看：{html_url}")
            })?
        }
    };

    println!("云端编译已启动：{html_url}");
    let start = Instant::now();
    let mut last_status = String::new();
    loop {
        let run: WorkflowRunSummary = github_get(
            http,
            &auth.token,
            &format!("{GITHUB_API_BASE}/repos/{repo_slug}/actions/runs/{run_id}"),
        )
        .send()
        .await
        .context("查询 GitHub Actions 编译状态失败")?
        .error_for_status()
        .context("查询 GitHub Actions 运行详情返回错误")?
        .json()
        .await?;

        if run.status != last_status {
            let elapsed = start.elapsed().as_secs();
            println!(
                "  [{}m{:02}s] 编译状态：{}",
                elapsed / 60,
                elapsed % 60,
                translate_run_status(&run.status)
            );
            last_status = run.status.clone();
        }
        if run.status == "completed" {
            ensure!(
                run.conclusion.as_deref() == Some("success"),
                "云端编译失败（结论：{}）。请打开日志页面查看详情：{}",
                run.conclusion.as_deref().unwrap_or("unknown"),
                run.html_url
            );
            break;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }

    download_and_verify_ipa(http, &auth.token, &repo_slug, run_id, &export.work_dir).await
}

async fn sync_workflow_file(
    http: &reqwest::Client,
    token: &str,
    repo_slug: &str,
    default_branch: &str,
) -> Result<()> {
    let contents_url =
        format!("{GITHUB_API_BASE}/repos/{repo_slug}/contents/.github/workflows/{WORKFLOW_FILE_NAME}");
    let existing = github_get(http, token, &contents_url).send().await?;
    let encoded_local = BASE64_STANDARD.encode(WORKFLOW_YML.as_bytes());

    if existing.status() == StatusCode::OK {
        let remote: ContentResponse = existing.json().await?;
        let cleaned_b64: String = remote.content.chars().filter(|c| !c.is_whitespace()).collect();
        let remote_bytes = BASE64_STANDARD
            .decode(&cleaned_b64)
            .context("无法解码仓库中现有的工作流文件")?;
        let remote_text = String::from_utf8_lossy(&remote_bytes);
        if !normalize_newlines_equal(&remote_text, WORKFLOW_YML) {
            let answer = crate::prompt(
                "私有仓库中存在旧版/不同版本的构建工作流，按 Enter 自动更新为最新版，输入其他字符取消",
            )?;
            ensure!(answer.is_empty(), "已取消操作。");
            github_put(http, token, &contents_url)
                .json(&json!({
                    "message": "Update Unity iOS build workflow",
                    "content": encoded_local,
                    "sha": remote.sha,
                    "branch": default_branch
                }))
                .send()
                .await?
                .error_for_status()
                .context("更新 GitHub 工作流文件失败")?;
        }
    } else if existing.status() == StatusCode::NOT_FOUND {
        println!("正在向私有仓库写入 iOS 编译工作流...");
        github_put(http, token, &contents_url)
            .json(&json!({
                "message": "Add Unity iOS build workflow",
                "content": encoded_local,
                "branch": default_branch
            }))
            .send()
            .await?
            .error_for_status()
            .context("写入 GitHub 工作流文件失败（请确认授权包含 workflow 权限）")?;
    } else {
        bail!("读取仓库工作流配置失败（HTTP {}）。", existing.status());
    }

    let workflow_api = format!(
        "{GITHUB_API_BASE}/repos/{repo_slug}/actions/workflows/{WORKFLOW_FILE_NAME}"
    );
    for attempt in 0..5 {
        let resp = github_get(http, token, &workflow_api).send().await?;
        if resp.status().is_success() {
            return Ok(());
        }
        if attempt == 4 {
            bail!(
                "GitHub 尚未启用该工作流，请打开 https://github.com/{repo_slug}/actions 检查一次后重试。"
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(())
}

async fn fetch_recent_runs(
    http: &reqwest::Client,
    token: &str,
    repo_slug: &str,
) -> Result<Vec<WorkflowRunSummary>> {
    let url = format!(
        "{GITHUB_API_BASE}/repos/{repo_slug}/actions/workflows/{WORKFLOW_FILE_NAME}/runs?event=workflow_dispatch&per_page=10"
    );
    let resp: RunsListResponse = github_get(http, token, &url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(resp.workflow_runs)
}

async fn fetch_recent_run_ids(
    http: &reqwest::Client,
    token: &str,
    repo_slug: &str,
) -> Result<Vec<u64>> {
    Ok(fetch_recent_runs(http, token, repo_slug)
        .await?
        .into_iter()
        .map(|r| r.id)
        .collect())
}

async fn download_and_verify_ipa(
    http: &reqwest::Client,
    token: &str,
    repo_slug: &str,
    run_id: u64,
    work_dir: &Path,
) -> Result<PathBuf> {
    println!("云端编译成功！正在下载并校验 IPA 产物...");
    let list_url = format!("{GITHUB_API_BASE}/repos/{repo_slug}/actions/runs/{run_id}/artifacts");
    let artifacts: ArtifactsListResponse = github_get(http, token, &list_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let artifact = artifacts
        .artifacts
        .into_iter()
        .find(|a| a.name == "unity-ios-unsigned-ipa" && !a.expired)
        .context("未在 GitHub Actions 运行产物中找到有效的 unity-ios-unsigned-ipa。")?;

    let ua = format!("Unity-IPA-Installer/{}", env!("CARGO_PKG_VERSION"));
    let no_redirect_client = reqwest::Client::builder()
        .user_agent(&ua)
        .redirect(Policy::none())
        .timeout(Duration::from_secs(300))
        .build()?;

    let dl_resp = github_get(&no_redirect_client, token, &artifact.archive_download_url)
        .send()
        .await
        .context("请求 GitHub 产物下载地址失败")?;
    let zip_bytes = if dl_resp.status().is_redirection() {
        let loc = dl_resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .context("GitHub 未返回产物下载重定向地址")?;
        http.get(loc)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .context("下载 IPA 压缩包失败")?
            .error_for_status()?
            .bytes()
            .await?
    } else {
        dl_resp.error_for_status()?.bytes().await?
    };

    let ipa_dir = work_dir.join("IPA");
    fs::create_dir_all(&ipa_dir)?;
    let mut archive = ZipArchive::new(Cursor::new(zip_bytes)).context("下载的产物不是有效的 ZIP")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let rel = entry
            .enclosed_name()
            .context("产物压缩包包含非法路径")?;
        let out_path = ipa_dir.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&out_path)?;
            io::copy(&mut entry, &mut out)?;
        }
    }

    let ipa_path = ipa_dir.join("unity-ios-unsigned.ipa");
    let sha_path = ipa_dir.join("unity-ios-unsigned.ipa.sha256");
    ensure!(ipa_path.is_file() && sha_path.is_file(), "产物缺少 IPA 或 SHA-256 校验文件。");

    let manifest = fs::read_to_string(&sha_path)?;
    let actual_sha = crate::unity::sha256_file(&ipa_path)?;
    ensure!(
        verify_sha256_manifest(&manifest, &actual_sha),
        "下载的 IPA 未通过 SHA-256 校验。"
    );

    println!("✅ IPA 已下载并通过校验：{}", ipa_path.display());
    Ok(ipa_path)
}

fn github_get(http: &reqwest::Client, token: &str, url: &str) -> reqwest::RequestBuilder {
    http.get(url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

fn github_post(http: &reqwest::Client, token: &str, url: &str) -> reqwest::RequestBuilder {
    http.post(url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

fn github_put(http: &reqwest::Client, token: &str, url: &str) -> reqwest::RequestBuilder {
    http.put(url)
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

fn translate_run_status(status: &str) -> &'static str {
    match status {
        "queued" => "排队等待 macOS 运行器...",
        "in_progress" => "正在云端编译 Xcode 工程...",
        "completed" => "编译流程已结束",
        _ => "处理中...",
    }
}

pub fn validate_repo_slug(slug: &str) -> bool {
    let Some((owner, repo)) = slug.split_once('/') else {
        return false;
    };
    if owner.is_empty() || repo.is_empty() || repo == "." || repo == ".." {
        return false;
    }
    let owner_ok = owner.as_bytes()[0].is_ascii_alphanumeric()
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let repo_ok = repo
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    owner_ok && repo_ok
}

pub fn normalize_newlines_equal(a: &str, b: &str) -> bool {
    a.replace("\r\n", "\n") == b.replace("\r\n", "\n")
}

pub fn strip_upload_url_template(upload_url: &str) -> &str {
    upload_url
        .split_once('{')
        .map(|(base, _)| base)
        .unwrap_or(upload_url)
}

pub fn verify_sha256_manifest(manifest_text: &str, actual_sha256: &str) -> bool {
    manifest_text.trim() == format!("{actual_sha256}  unity-ios-unsigned.ipa")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_repo_slugs() {
        assert!(validate_repo_slug("octocat/unity-ios-builds"));
        assert!(validate_repo_slug("user-1/Repo.Name_2"));
        assert!(!validate_repo_slug("invalid"));
        assert!(!validate_repo_slug("-bad/repo"));
        assert!(!validate_repo_slug("owner/.."));
        assert!(!validate_repo_slug("owner/a/b"));
    }

    #[test]
    fn normalizes_crlf_and_lf() {
        assert!(normalize_newlines_equal("a\r\nb\r\n", "a\nb\n"));
        assert!(!normalize_newlines_equal("a\nb\n", "a\nc\n"));
    }

    #[test]
    fn strips_upload_url_template() {
        assert_eq!(
            strip_upload_url_template("https://uploads.github.com/repos/a/b/releases/1/assets{?name,label}"),
            "https://uploads.github.com/repos/a/b/releases/1/assets"
        );
    }

    #[test]
    fn checks_oauth_scopes() {
        assert!(has_required_scopes("repo, workflow"));
        assert!(has_required_scopes("read:user, repo, workflow"));
        assert!(!has_required_scopes("repo"));
        assert!(!has_required_scopes("workflow"));
    }

    #[test]
    fn verifies_sha256_manifest() {
        let sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(verify_sha256_manifest(&format!("{sha}  unity-ios-unsigned.ipa\n"), sha));
        assert!(!verify_sha256_manifest(&format!("{sha}  other.ipa"), sha));
    }

    #[test]
    fn token_dpapi_round_trip() -> Result<()> {
        let token = "example-synthetic-token-123456";
        let encrypted = crate::pairing::crypt(token.as_bytes(), true)?;
        let decrypted = crate::pairing::crypt(&encrypted, false)?;
        assert_eq!(std::str::from_utf8(&decrypted)?, token);
        Ok(())
    }
}
