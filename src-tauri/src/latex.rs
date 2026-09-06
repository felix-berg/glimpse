use std::sync::atomic::AtomicBool;
use std::{fs, thread};
use std::hash::{Hash, DefaultHasher, Hasher};
use std::process::Command;
use std::sync::{Mutex, Arc, MutexGuard};
use cocoa::appkit::NSOpenGLPixelFormatAttribute::NSOpenGLPFAAllRenderers;
use futures::TryFutureExt;
use futures::channel::oneshot;
use tauri::{AppHandle, Manager};  
use std::collections::{HashMap};
use std::future::{Future};
use futures::future::{Shared, BoxFuture, FutureExt};
use std::time::{Duration};

pub trait LatexMathCompiler {
    fn set_preamble(&self, content: String) -> Result<(), String>;
    fn math_to_svg(&self, math: &String) -> impl Future<Output = Result<String, String>>;
}

type SharedFuture<T> = Shared<oneshot::Receiver<T>>;

struct RenderJob {
    pub future: Shared<oneshot::Receiver<Result<(), String>>>,
    pub math_blocks: Arc<Mutex<Vec<String>>>,
    finished: Arc<AtomicBool>
}

impl RenderJob {
    pub fn new(initial_vec: Vec<String>, preamble: String, base_path: std::path::PathBuf) -> Self {
        let (tx, rx) = oneshot::channel::<Result<(), String>>();

        let shared_rx = rx.shared();

        let math_blocks = Arc::new(Mutex::new(initial_vec));
        let finished = Arc::new(AtomicBool::new(false));

        let math_blocks2 = math_blocks.clone();
        let finished2 = finished.clone();

        tokio::spawn(async move {
            thread::sleep(Duration::from_millis(100));

            let math_blocks = math_blocks2.lock().unwrap();
            finished2.store(true, std::sync::atomic::Ordering::Relaxed);

            println!("Rendering {} math blocks", math_blocks.len());
            tx.send(compile(&math_blocks, false, &preamble, &base_path))
        });

        return RenderJob {
            future: shared_rx,
            math_blocks: math_blocks,
            finished: finished
        }
    }
}

pub struct LatexMathCompilerImpl {
    preamble: Mutex<String>,
    current_renders: Arc<Mutex<HashMap<String, SharedFuture<Result<(), String>>>>>,
    base_path: std::path::PathBuf,
    jobs: Mutex<Vec<RenderJob>>,
}

const DEFAULT_PREAMBLE: &str = r#"
    \usepackage{amsmath}
    \usepackage{amssymb}
    \usepackage{amsfonts}
"#;

pub fn read_preamble(app: &AppHandle) -> String {
    match app.path().app_config_dir() {
        Ok(config_dir) => {
            let preamble_path = config_dir.join("preamble.tex");

            if preamble_path.exists() {
                match fs::read_to_string(preamble_path) {
                    Ok(content) => return content, // Found user preamble
                    Err(_) => println!("Error reading preamble file"),
                }
            }
        }
        Err(e) => println!("Could not resolve app config dir: {}", e),
    }

    // Fallback
    DEFAULT_PREAMBLE.to_string()
}


static ID_COUNTER: Mutex<i32> = Mutex::new(0);
fn get_fresh_basename() -> String {
    let mut guard = ID_COUNTER.lock().unwrap();
    let res = guard.clone();
    *guard = res + 1;
    res.to_string()
}

fn hash<T: Hash>(t: &T) -> String {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    let i = s.finish();
    i.to_string()
}

fn hash_math(math: &String, preamble_hash: &String) -> String {
    let str = format!("{}-{}", math, preamble_hash);
    hash(&str)
}

fn create_tex_file(
    tex_dir: &std::path::PathBuf,
    basename: &String,
    contents: &String,
) -> Result<(), String> {
    let tex_path = tex_dir.join(format!("{}.tex", basename));
    std::fs::write(&tex_path, contents).map_err(|e| e.to_string())
}

// this runs the `latex` compiler on ${directory}/${basename}.tex with output directory ${directory}
fn run_latex_compiler(
    tex_dir: &std::path::PathBuf,
    basename: &String,
) -> Result<(), String> {
    let tex = tex_dir.join(format!("{}.tex", basename));
    let output = Command::new("latex")
        .args([
            "-interaction=nonstopmode",
            "-output-directory",
            tex_dir.to_str().unwrap(),
            tex.to_str().unwrap(),
        ])
        .output()
        .map_err(|e| format!("`latex` command failed: {}", e))?;

    if !output.status.success() {
        let log = fs::read_to_string(tex_dir.join(format!("{}.log", basename)))
            .unwrap_or_else(|_| "Could not read LaTeX log.".to_string());

        return Err(format!("LaTeX compilation failed. See log:\n\n{}", log));
    }

    Ok(())
}

fn run_dvisvgm(
    tex_dir: &std::path::PathBuf,
    svg_dir: &std::path::PathBuf,
    basename: &String,
) -> Result<(), String> {
    let dvi_path = tex_dir.join(format!("{}.dvi", basename));
    let output_pattern = svg_dir.join(format!("{}-%3p.svg", basename));
    let dvisvgm_output = Command::new("dvisvgm")
        .args([
            "--zoom=1.1", // Seems to fix scaling issues
            "--exact-bbox",
            "--no-fonts",
            "--page=-",
            format!("--output={}", output_pattern.to_str().unwrap()).as_str(),
            dvi_path.to_str().unwrap(),
        ])
        .output()
        .map_err(|e| format!("`dvisvgm` command failed: {}", e))?;

    println!("{}", String::from_utf8_lossy(&dvisvgm_output.stdout));
    println!("{}", String::from_utf8_lossy(&dvisvgm_output.stderr));

    if !dvisvgm_output.status.success() {
        return Err(format!(
            "dvisvgm conversion failed: {}",
            String::from_utf8_lossy(&dvisvgm_output.stderr)
        ));
    }

    Ok(())
}

fn rename_svgs(
    math_blocks: &Vec<String>,
    svg_dir: &std::path::PathBuf,
    basename: &String,
    preamble_hash: &String,
) -> Result<(), String> {
    // rename output svgs to be hash of input
    for i in 0..math_blocks.len() {
        let old_name = format!("{}-{:0>3}.svg", basename, (i + 1));
        let new_name = format!("{}.svg", hash_math(math_blocks.get(i).unwrap(), preamble_hash));

        let [old_path, new_path] = [&old_name, &new_name].map(|n| svg_dir.join(n));

        match std::fs::rename(&old_path, &new_path) {
            Err(err) => return Err(format!("Renaming {} to {} failed:\n {}", &old_name, &new_name, err.to_string())),
            _ => ()
        }
    }

    Ok(())
}

fn remove_scratch_files(
    tex_dir: &std::path::PathBuf,
    basename: &String,
) -> Result<(), String> {
    for f in vec![
        format!("{}.tex", &basename),
        format!("{}.aux", &basename),
        format!("{}.dvi", &basename),
        format!("{}.log", &basename),
    ] {
        if std::fs::remove_file(tex_dir.join(&f)).is_err() {
            // TODO: handle this case
        }
    }

    Ok(())
}

fn compile(
    math_blocks: &Vec<String>,
    display_mode: bool,
    preamble_content: &str,
    base_path: &std::path::PathBuf,
) -> Result<(), String> {

    let tex_dir = base_path.join("tex");
    let svg_dir = base_path.join("svg");
    let basename = get_fresh_basename();
    let preamble_hash = hash(&preamble_content.to_string());

    let tex_content = generate_latex_content(&math_blocks, display_mode, preamble_content);
    let svg_result: Result<(), String> = Ok(())
        .and_then(|_| create_tex_file(&tex_dir, &basename, &tex_content))
        .and_then(|_| run_latex_compiler(&tex_dir, &basename))
        .and_then(|_| run_dvisvgm(&tex_dir, &svg_dir, &basename))
        .and_then(|_| rename_svgs(&math_blocks, &svg_dir, &basename, &preamble_hash));

    let clean_result = remove_scratch_files(&tex_dir, &basename);
    if clean_result.is_err() { todo!("handle cleaning failed") }

    svg_result
}

fn create_equation_page(math: &str) -> String {
    return format!("\\begin{{page}}\\begin{{equation*}}\\textcolor[RGB]{{244, 244, 244}}{{\\rule[0pt]{{1pt}}{{1pt}}}}\n{}\n\\end{{equation*}}\n\\end{{page}}", math);
}

// TODO: handle display mode
fn generate_latex_content(math_blocks: &Vec<String>, display_mode: bool, preamble_content: &str) -> String {
    let pages: Vec<String> = math_blocks.iter().map(|math| create_equation_page(math.as_str())).collect();
    let body = pages.join("\n");

    format!(
        r#"
            \documentclass[dvisvgm, preview, 12pt, multi=page]{{standalone}}
            \usepackage[utf8]{{inputenc}}
            \usepackage{{xcolor}}
            % --- Preamble below ---
            {}
            % --- Input below ---
            \begin{{document}}
            {}
            \end{{document}}
        "#,
        preamble_content,
        // TODO: make it do text-size when not in display mode
        body.as_str()
    )
}

fn svg_lookup(svg_dir: &std::path::PathBuf, math: &String, preamble_hash: &String) -> Option<String> {
    let svg_name = hash_math(math, preamble_hash);
    let svg_path = svg_dir.join(format!("{}.svg", svg_name));

    match std::fs::exists(&svg_path) {
        Ok(false) => return None,
        Ok(true) => (),
        Err(_) => todo!("handle exists failure in cache lookup"),
    }

    match std::fs::read_to_string(svg_path) {
        Ok(svg_content) => Some(svg_content),
        Err(_) => todo!("handle read failure in cache lookup")
    }
}

impl LatexMathCompilerImpl {
    pub fn new(base_path: std::path::PathBuf, initial_preamble: String) -> Self {
        return Self {
            preamble: Mutex::new(initial_preamble),
            current_renders: Arc::new(Mutex::new(HashMap::new())),
            base_path: base_path,
            jobs: Mutex::new(Vec::new())
        }
    }

    fn render_at_some_point(&self, math: &String) -> SharedFuture<Result<(), String>> {
        let mut jobs = self.jobs.lock().unwrap();
        for i in 0 .. jobs.len() {
            let job = jobs.get(i).unwrap();

            // aquire lock eagerly
            let mut math_blocks = job.math_blocks.lock().unwrap();

            if math_blocks.len() < 10 && !job.finished.load(std::sync::atomic::Ordering::Relaxed) {
                math_blocks.push(math.clone());
                return job.future.clone();
            }
        };

        jobs.retain(|job| !job.finished.load(std::sync::atomic::Ordering::Relaxed));

        jobs.push(RenderJob::new(vec![math.clone()], self.preamble.lock().unwrap().clone(), self.base_path.clone()));

        jobs.last().unwrap().future.clone()
    }

    fn obtain_render_future(&self, math: &String) -> SharedFuture<Result<(), String>> {
        let mut future_map = self.current_renders.lock().unwrap();
        if let Some(shared_future) = future_map.get(math) {
            // this exact math is currently being rendered
            shared_future.clone()
        } else {
            let future = self.render_at_some_point(math);
            future_map.insert(math.clone(), future.clone());
            future
        }
    }
}

impl LatexMathCompiler for LatexMathCompilerImpl {
    fn set_preamble(&self, content: String) -> Result<(), String> {
        { // set preamble
            let mut preamble = self.preamble.lock().unwrap();
            *preamble = content;
        }
        Ok(())
    }

    async fn math_to_svg(&self, math: &String) -> Result<String, String> {
        let svg_dir = &self.base_path.join("svg");

        {
            let preamble_hash = hash(&self.preamble.lock().unwrap().clone());
            if let Some(svg) = svg_lookup(&svg_dir, &math, &preamble_hash) {
                return Ok(svg)
            }
        }

        let preamble_hash = hash(&self.preamble.lock().unwrap().clone());
        match self.obtain_render_future(math).await {
            Ok(Ok(())) => 
                svg_lookup(&svg_dir, &math, &preamble_hash)
                    .ok_or_else(|| todo!("fix this bug: no svg after compilation with no errors")),
            Ok(Err(e)) => Err(e),
            Err(_) => todo!()
        }
    }
}
