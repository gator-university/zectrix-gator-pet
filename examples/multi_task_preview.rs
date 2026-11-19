use anyhow::{Context, Result};
use dsh_pet::{
    layout,
    model::{Event, Model, ProgressSource, Status, WaitingKind},
    raster::{self, Fonts},
};
use std::{env, fs, path::Path};

const NOW: i64 = 200;

fn event(
    task_id: &str,
    title: &str,
    status: Status,
    waiting_kind: Option<WaitingKind>,
) -> Event {
    Event {
        task_id: task_id.into(),
        thread_id: None,
        seq: 1,
        status,
        waiting_kind,
        title: title.into(),
        detail: String::new(),
        progress: None,
        progress_source: ProgressSource::Agent,
        progress_steps: None,
        agent: "Preview".into(),
    }
}

fn add_task(
    model: &mut Model,
    task_id: &str,
    status: Status,
    waiting_kind: Option<WaitingKind>,
    index: usize,
) -> Result<()> {
    let title = format!("检查多任务布局 {task_id} 的状态与进度");
    model.apply(
        event(task_id, &title, status, waiting_kind),
        "preview",
        100 + index as i64,
    )?;
    Ok(())
}

fn working_model(count: usize) -> Result<Model> {
    let mut model = Model::default();
    for index in 0..count {
        add_task(
            &mut model,
            &format!("working-{index}"),
            Status::Working,
            None,
            index,
        )?;
    }
    Ok(model)
}

fn render(model: &Model, path: &Path, fonts: &Fonts) -> Result<()> {
    let scene = layout::draw(model, NOW);
    let png = raster::to_png(&scene, fonts)?;
    fs::write(path, png).with_context(|| format!("写入预览失败：{}", path.display()))?;
    Ok(())
}

fn main() -> Result<()> {
    let output = env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .context("需要指定预览输出目录")?;
    fs::create_dir_all(&output)?;
    let fonts = Fonts::load()?;

    for count in [2usize, 3, 4] {
        let mut model = working_model(count)?;
        render(
            &model,
            &output.join(format!("working-{count}-page-1.png")),
            &fonts,
        )?;
        if count > 3 {
            model.cycle(1, NOW);
            render(
                &model,
                &output.join(format!("working-{count}-page-2.png")),
                &fonts,
            )?;
        }
    }

    let mut model = Model::default();
    add_task(
        &mut model,
        "question",
        Status::Waiting,
        Some(WaitingKind::Question),
        0,
    )?;
    add_task(
        &mut model,
        "permission",
        Status::Waiting,
        Some(WaitingKind::Permission),
        1,
    )?;
    for index in 0..3 {
        add_task(
            &mut model,
            &format!("waiting-work-{index}"),
            Status::Working,
            None,
            index + 2,
        )?;
    }
    render(
        &model,
        &output.join("waiting-5-page-1.png"),
        &fonts,
    )?;
    model.cycle(1, NOW);
    render(
        &model,
        &output.join("waiting-5-page-2.png"),
        &fonts,
    )?;

    Ok(())
}
