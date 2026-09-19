//! /备份 与 /恢复 命令（对应原版 StandaloneBackup）
//! 打包框架 arona.yml + onebot.yml + 本插件 config/bluearchive/arona.yml + 本插件 data 目录；
//! 恢复时校验后覆盖并热重载。

use crate::db;
use arona::config;
use arona::config::plugin_config;
use arona::quartz;
use arona::runtime::message::OutgoingMessage;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 插件自己的 arona.yml 在压缩包里的条目名（框架那份叫 arona.yml，两者不能撞名）
const PLUGIN_CONFIG_ENTRY: &str = "plugin-arona.yml";

/// /备份 [list|列表]
pub fn backup(arguments: &[String]) -> OutgoingMessage {
    match arguments
        .first()
        .map(|s| s.to_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "list" | "列表" => list_backups(),
        "" => create_backup(),
        _ => OutgoingMessage::text("用法: /备份 创建备份; /备份 list 查看已有备份"),
    }
}

/// /恢复 <备份文件名>
pub fn restore(name: Option<&str>) -> OutgoingMessage {
    let Some(name) = name.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return OutgoingMessage::text("用法: /恢复 <备份文件名>，可用 /备份 list 查看");
    };
    if name.contains('/') || name.contains('\\') || name == ".." {
        return OutgoingMessage::text(format!("非法的备份文件名: {name}"));
    }
    let zip_file = crate::backups_dir().join(name);
    if !zip_file.is_file() {
        return OutgoingMessage::text(format!("备份文件不存在: {name}"));
    }
    let temp_dir = std::env::temp_dir().join(format!("arona-restore-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let result = do_restore(&zip_file, &temp_dir, name);
    let _ = std::fs::remove_dir_all(&temp_dir);
    result
}

fn do_restore(zip_file: &Path, temp_dir: &Path, name: &str) -> OutgoingMessage {
    let entries = match extract(zip_file, temp_dir) {
        Ok(entries) => entries,
        Err(err) => return OutgoingMessage::text(format!("恢复失败: {err}")),
    };
    let restored_arona = temp_dir.join("arona.yml");
    if !entries.iter().any(|e| e == "arona.yml") || !restored_arona.is_file() {
        return OutgoingMessage::text("备份中缺少 arona.yml，无法恢复");
    }
    if let Err(err) = config::arona::load(&restored_arona) {
        return OutgoingMessage::text(format!("备份的 arona.yml 解析失败: {err}"));
    }
    arona::runtime::log::info(format!("开始恢复备份: {name}"));
    let _ = quartz::pause_all();
    db::close();
    let arona_file = config::standalone::default_file();
    if let Err(err) = std::fs::copy(&restored_arona, &arona_file) {
        return fail_restore(format!("覆盖 arona.yml 失败: {err}"));
    }
    let restored_onebot = temp_dir.join("onebot.yml");
    if restored_onebot.is_file() {
        let onebot_file = config::onebot::default_file();
        if let Err(err) = std::fs::copy(&restored_onebot, &onebot_file) {
            return fail_restore(format!("覆盖 onebot.yml 失败: {err}"));
        }
    }
    if let Err(err) = restore_data_dir(&temp_dir.join("data")) {
        return fail_restore(format!("恢复数据目录失败: {err}"));
    }
    // 插件自己的配置区：旧备份里没有这一项，缺省时保持现状不动
    let restored_plugin_config = temp_dir.join(PLUGIN_CONFIG_ENTRY);
    if restored_plugin_config.is_file() {
        let target = plugin_config::config_file(crate::PLUGIN_ID);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::copy(&restored_plugin_config, &target) {
            return fail_restore(format!("覆盖插件配置失败: {err}"));
        }
        plugin_config::reload_all();
    }
    let _ = config::standalone::init(arona_file);
    let _ = db::start();
    let _ = quartz::resume_all();
    let notice = if restored_onebot.is_file() {
        "\n注: onebot.yml 连接配置已恢复, 重启后生效"
    } else {
        ""
    };
    OutgoingMessage::text(format!("恢复完成: {name}{notice}"))
}

fn fail_restore(err: String) -> OutgoingMessage {
    let _ = db::start();
    let _ = quartz::resume_all();
    OutgoingMessage::text(format!("恢复失败: {err}"))
}

fn create_backup() -> OutgoingMessage {
    let arona_file = config::standalone::default_file();
    if !arona_file.is_file() {
        return OutgoingMessage::text("arona.yml 不存在，无法备份");
    }
    let backups_dir = crate::backups_dir();
    let _ = std::fs::create_dir_all(&backups_dir);
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let zip_file = backups_dir.join(format!("arona-backup-{stamp}.zip"));
    let absolute = std::env::current_dir()
        .map(|cwd| cwd.join(&zip_file))
        .unwrap_or_else(|_| zip_file.clone());
    let _ = quartz::pause_all();
    db::close();
    let result = (|| -> Result<(), String> {
        let file = File::create(&zip_file).map_err(|err| format!("创建备份文件失败: {err}"))?;
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        write_entry(&mut zip, options, "arona.yml", &arona_file)?;
        let onebot_file = config::onebot::default_file();
        if onebot_file.is_file() {
            write_entry(&mut zip, options, "onebot.yml", &onebot_file)?;
        }
        let own_config = plugin_config::config_file(crate::PLUGIN_ID);
        if own_config.is_file() {
            write_entry(&mut zip, options, PLUGIN_CONFIG_ENTRY, &own_config)?;
        }
        let data_dir = crate::data_dir();
        if data_dir.is_dir() {
            let backups_dir = crate::backups_dir();
            for file in collect_files(&data_dir) {
                // 备份目录本身就在 data 下：不排掉的话每次备份都会把历史备份再套一层
                if file.starts_with(&backups_dir) {
                    continue;
                }
                let relative = file
                    .strip_prefix(&data_dir)
                    .unwrap_or(&file)
                    .to_string_lossy()
                    .replace('\\', "/");
                write_entry(&mut zip, options, &format!("data/{relative}"), &file)?;
            }
        }
        zip.finish().map_err(|err| format!("完成备份失败: {err}"))?;
        Ok(())
    })();
    let _ = db::start();
    let _ = quartz::resume_all();
    match result {
        Ok(()) => OutgoingMessage::text(format!(
            "备份完成: {}\n路径: {}",
            zip_file.file_name().unwrap_or_default().to_string_lossy(),
            absolute.display()
        )),
        Err(err) => {
            let _ = std::fs::remove_file(&zip_file);
            OutgoingMessage::text(format!("备份失败: {err}"))
        }
    }
}

fn write_entry<W: Write + std::io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    options: zip::write::SimpleFileOptions,
    entry_name: &str,
    file: &Path,
) -> Result<(), String> {
    let mut reader =
        File::open(file).map_err(|err| format!("打开 {} 失败: {err}", file.display()))?;
    zip.start_file(entry_name, options)
        .map_err(|err| format!("写入备份条目失败: {err}"))?;
    std::io::copy(&mut reader, zip).map_err(|err| format!("写入备份数据失败: {err}"))?;
    Ok(())
}

fn list_backups() -> OutgoingMessage {
    let backups_dir = crate::backups_dir();
    if !backups_dir.is_dir() {
        return OutgoingMessage::text("还没有任何备份");
    }
    let mut names: Vec<String> = std::fs::read_dir(&backups_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_file())
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".zip"))
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    if names.is_empty() {
        return OutgoingMessage::text("还没有任何备份");
    }
    OutgoingMessage::text(format!("已有备份:\n{}", names.join("\n")))
}

fn collect_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(collect_files(&path));
        } else if path.is_file() {
            out.push(path);
        }
    }
    out
}

fn extract(zip_file: &Path, target: &Path) -> Result<Vec<String>, String> {
    let file = File::open(zip_file).map_err(|err| format!("打开备份文件失败: {err}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|err| format!("读取备份文件失败: {err}"))?;
    let mut names = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|err| format!("读取备份条目失败: {err}"))?;
        let name = entry.name().to_string();
        let Some(enclosed) = entry.enclosed_name() else {
            return Err(format!("非法的备份条目: {name}"));
        };
        let dest = target.join(enclosed);
        if entry.is_dir() {
            let _ = std::fs::create_dir_all(&dest);
            names.push(name);
            continue;
        }
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut out = File::create(&dest).map_err(|err| format!("创建恢复文件失败: {err}"))?;
        std::io::copy(&mut entry, &mut out).map_err(|err| format!("写入恢复文件失败: {err}"))?;
        names.push(name);
    }
    Ok(names)
}

fn restore_data_dir(restored_data: &Path) -> Result<(), String> {
    if !restored_data.is_dir() {
        return Ok(());
    }
    let data_dir = crate::data_dir();
    for file in collect_files(restored_data) {
        let relative = file
            .strip_prefix(restored_data)
            .map_err(|err| err.to_string())?;
        let target = data_dir.join(relative);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::copy(&file, &target)
            .map_err(|err| format!("恢复 {} 失败: {err}", file.display()))?;
    }
    Ok(())
}
