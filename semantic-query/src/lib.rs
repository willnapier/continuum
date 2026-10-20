use anyhow::{ensure, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use unicode_normalization::UnicodeNormalization;

#[derive(Parser)]
#[command(
    version,
    about = "Semantic search using the existing FAISS index; no implicit index rebuild"
)]
struct Args {
    #[arg(long, required_unless_present = "file", conflicts_with = "file")]
    text: Option<String>,
    #[arg(long)]
    file: Option<PathBuf>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    config: Option<PathBuf>,
    /// Machine-readable array, suitable for the Continuum MCP caller.
    #[arg(long)]
    json: bool,
    /// Require every indexed source to be an archive messages file inside this root.
    #[arg(long)]
    root: Option<PathBuf>,
}
#[derive(Deserialize)]
struct Config {
    database: Database,
    openai: OpenAi,
    query: Query,
}
#[derive(Deserialize)]
struct Database {
    index_path: PathBuf,
    metadata_path: PathBuf,
}
#[derive(Deserialize)]
struct OpenAi {
    model: String,
}
#[derive(Deserialize)]
struct Query {
    max_results: usize,
    similarity_threshold: f64,
    result_format: String,
}
#[derive(Deserialize)]
struct Metadata {
    file_paths: Vec<PathBuf>,
}
#[derive(Deserialize)]
struct Neighbor {
    index: usize,
    similarity: f64,
}
#[derive(Serialize)]
struct Hit {
    path: PathBuf,
    similarity: f64,
    title: String,
    snippet: String,
}
fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").context("HOME missing")?,
    ))
}
fn expand(p: &Path) -> Result<PathBuf> {
    if p == Path::new("~") {
        return home();
    }
    if let Ok(s) = p.strip_prefix("~") {
        return Ok(home()?.join(s));
    }
    Ok(p.into())
}
fn bounded_text(p: &Path, cap: u64) -> Result<String> {
    use std::io::Read;
    let f = fs::File::open(p)?;
    ensure!(f.metadata()?.is_file(), "expected regular file");
    let mut text = String::new();
    f.take(cap + 1).read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 <= cap,
        "file exceeds semantic query size limit"
    );
    Ok(text)
}
fn content_without_frontmatter(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("---\n") {
        if let Some((_, body)) = rest.split_once("\n---\n") {
            return body.trim().into();
        }
    }
    s.trim().into()
}
fn normalized(mut vector: Vec<f64>) -> Result<Vec<f32>> {
    ensure!(
        !vector.is_empty() && vector.len() <= 65536 && vector.iter().all(|x| x.is_finite()),
        "invalid embedding vector"
    );
    let norm = vector.iter().map(|x| x * x).sum::<f64>().sqrt();
    ensure!(norm.is_finite() && norm > 0.0, "invalid embedding norm");
    for x in &mut vector {
        *x /= norm;
    }
    Ok(vector.into_iter().map(|x| x as f32).collect())
}
fn embedding(text: &str, model: &str) -> Result<Vec<f32>> {
    let key = std::env::var("OPENAI_API_KEY")
        .context("OPENAI_API_KEY required for configured semantic embeddings")?;
    ensure!(!key.is_empty(), "OPENAI_API_KEY is empty");
    let base =
        std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    // Only the caller's explicitly configured embedding route. No subscription/API fallback.
    let response = agent
        .post(&format!("{}/embeddings", base.trim_end_matches('/')))
        .set("Authorization", &format!("Bearer {key}"))
        .send_json(serde_json::json!({"input":text,"model":model}))
        .map_err(|_| anyhow::anyhow!("embedding request failed (configured endpoint)"))?;
    #[derive(Deserialize)]
    struct Data {
        embedding: Vec<f64>,
    }
    #[derive(Deserialize)]
    struct Response {
        data: Vec<Data>,
    }
    use std::io::Read;
    let mut bytes = vec![];
    response
        .into_reader()
        .take(2_000_001)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 2_000_000, "embedding response exceeds limit");
    let mut response: Response =
        serde_json::from_slice(&bytes).context("invalid embedding response")?;
    ensure!(response.data.len() == 1, "expected one embedding");
    normalized(response.data.remove(0).embedding)
}
pub fn run(continuum: bool) -> Result<()> {
    let args = Args::parse();
    let config_path = args.config.unwrap_or(home()?.join(if continuum {
        ".local/share/semantic-search-continuum/config.yaml"
    } else {
        ".local/share/semantic-search/config.yaml"
    }));
    let config_path = expand(&config_path)?
        .canonicalize()
        .context("semantic config unavailable")?;
    let configured_path = |p: &Path| -> Result<PathBuf> {
        let p = expand(p)?;
        Ok(if p.is_absolute() {
            p
        } else {
            config_path.parent().unwrap().join(p)
        })
    };
    let config: Config = serde_yaml::from_str(&bounded_text(&config_path, 1_000_000)?)
        .context("invalid semantic config")?;
    let max = args.limit.unwrap_or(config.query.max_results);
    ensure!((1..=1000).contains(&max), "limit must be 1..1000");
    ensure!(
        config.query.similarity_threshold.is_finite(),
        "invalid threshold"
    );
    let index = configured_path(&config.database.index_path)?;
    ensure!(
        fs::metadata(&index)
            .context("semantic index unavailable; no automatic rebuild")?
            .is_file(),
        "index must be a file"
    );
    let metadata: Metadata = serde_json::from_str(&bounded_text(
        &configured_path(&config.database.metadata_path)?,
        32 * 1024 * 1024,
    )?)
    .context("invalid semantic metadata")?;
    ensure!(
        !metadata.file_paths.is_empty(),
        "semantic index metadata is empty"
    );
    let root = args.root;
    let archive = root
        .as_deref()
        .map(continuum_mcp::Archive::open)
        .transpose()?;
    // Validate ALL metadata before any file content or embedding request.
    if let Some(a) = &archive {
        for p in &metadata.file_paths {
            a.validate_source_path(p)
                .map_err(|e| anyhow::anyhow!("invalid indexed source {}: {e}", p.display()))?;
        }
    }
    let description = args
        .text
        .clone()
        .unwrap_or_else(|| args.file.as_ref().unwrap().display().to_string());
    let text = match args.text {
        Some(t) => t,
        None => {
            let file = args.file.context("text or file required")?;
            if let Some(a) = &archive {
                a.source_text(&file)?
            } else {
                content_without_frontmatter(&bounded_text(&file, 32 * 1024 * 1024)?)
            }
        }
    };
    ensure!(!text.trim().is_empty(), "empty semantic query");
    let python = std::env::var_os("SEMANTIC_SEARCH_PYTHON")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".local/share/semantic-search-runtime/venv/bin/python3"));
    ensure!(
        python.is_file(),
        "host-local FAISS runtime missing; see semantic-query/README.md"
    );
    let vector = embedding(&text, &config.openai.model)?;
    let mut cmd = Command::new(python);
    cmd.args(["-I", "-c", include_str!("../faiss_bridge.py")])
        .arg(index);
    cmd.current_dir("/");
    cmd.env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_BASE_URL");
    let request = serde_json::to_vec(
        &serde_json::json!({"vector":vector,"limit":max,"count":metadata.file_paths.len()}),
    )?;
    let out = continuum_mcp::bounded_output(&mut cmd, Some(request), 35)?;
    ensure!(
        out.status.success(),
        "FAISS binding failed: {}",
        String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(2000)
            .collect::<String>()
    );
    let neighbors: Vec<Neighbor> =
        serde_json::from_slice(&out.stdout).context("invalid FAISS binding response")?;
    let mut hits = vec![];
    for n in neighbors {
        ensure!(n.similarity.is_finite(), "invalid FAISS similarity");
        let path = metadata
            .file_paths
            .get(n.index)
            .context("FAISS index outside metadata")?
            .clone();
        if n.similarity < config.query.similarity_threshold {
            continue;
        }
        let content = if let Some(a) = &archive {
            a.source_text(&path)?
        } else {
            content_without_frontmatter(&bounded_text(&path, 32 * 1024 * 1024)?)
        };
        let title = content
            .lines()
            .take(10)
            .find_map(|s| {
                s.strip_prefix('#')
                    .map(|s| s.trim_start_matches('#').trim().nfc().collect::<String>())
            })
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .nfc()
                    .collect()
            });
        let snippet = content
            .split("\n\n")
            .find(|s| !s.trim().is_empty())
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        hits.push(Hit {
            path,
            similarity: n.similarity,
            title,
            snippet,
        });
    }
    if args.json {
        println!("{}", serde_json::to_string(&hits)?);
    } else if hits.is_empty() {
        println!("No results found for: {description}");
    } else {
        println!("Results for: \"{description}\"\n{}", "─".repeat(80));
        for h in hits {
            let name = h
                .path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .nfc()
                .collect::<String>();
            println!("{:.2}  {}", h.similarity, name);
            if config.query.result_format == "detailed" {
                println!(
                    "      {}\n",
                    h.snippet
                        .replace('\n', " ")
                        .chars()
                        .take(100)
                        .collect::<String>()
                );
            }
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_vectors() {
        for v in [vec![], vec![0.0], vec![f64::NAN], vec![f64::INFINITY]] {
            assert!(normalized(v).is_err());
        }
    }
    #[test]
    fn normalizes_and_preserves_content() {
        assert_eq!(normalized(vec![3., 4.]).unwrap(), vec![0.6, 0.8]);
        assert_eq!(
            content_without_frontmatter("---\nkey: value\n---\nbody"),
            "body"
        );
    }
}
