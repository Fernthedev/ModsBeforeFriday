use std::{fs::{File, OpenOptions}, io::{BufReader, Cursor, Read, Seek, Write}, path::{Path, PathBuf}, process::Command, time::Instant};

use anyhow::{Context, Result, anyhow};
use log::{error, info, warn};
use crate::{axml::{AxmlReader, AxmlWriter}, copy_stream_progress, external_res::{self, Diff, VersionDiffs}, requests::{AppInfo, ModLoader}, zip::{self, ZIP_CRC}, ModTag, APK_ID, APP_DATA_PATH, APP_OBB_PATH, TEMP_PATH};
use crate::manifest::{ManifestMod, ResourceIds};
use crate::zip::{signing, FileCompression, ZipFile};

const DEBUG_CERT_PEM: &[u8] = include_bytes!("debug_cert.pem");
const LIB_MAIN: &[u8] = include_bytes!("../libs/libmain.so");
const MODLOADER: &[u8] = include_bytes!("../libs/libsl2.so");
const MODLOADER_NAME: &str = "libsl2.so";
const MOD_TAG_PATH: &str = "modded.json";

const LIB_MAIN_PATH: &str = "lib/arm64-v8a/libmain.so";
const LIB_UNITY_PATH: &str = "lib/arm64-v8a/libunity.so";
const DIFF_DOWNLOAD_ATTEMPTS: u32 = 3;

// Mods the currently installed version of the given app.
pub fn mod_current_apk(app_info: &AppInfo) -> Result<()> {
    let temp_path = Path::new(TEMP_PATH);
    std::fs::create_dir_all(TEMP_PATH)?;

    info!("Downloading unstripped libunity.so (this could take a minute)");
    let libunity_path = save_libunity(temp_path, app_info).context("Failed to save libunity.so")?;

    info!("Copying APK to temporary location");
    let temp_apk_path = temp_path.join("mbf-tmp.apk");
    std::fs::copy(&app_info.path, &temp_apk_path).context("Failed to copy APK to temp")?;

    info!("Saving OBB files");
    let obb_backup = temp_path.join("obbs");
    let obb_backups = save_obbs(Path::new(APP_OBB_PATH), &obb_backup)?;

    let player_data_backup = temp_path.join("PlayerData.backup");

    let player_data_path = Path::new(APP_DATA_PATH).join("PlayerData.dat");
    let backed_up_data = if player_data_path.exists() {
        info!("Backing up player data");
        std::fs::copy(&player_data_path, &player_data_backup)?;
        true
    }   else    {
        info!("No player data to save");
        false
    };

    info!("Patching APK at {:?}", temp_path);
    patch_apk_in_place(&temp_apk_path, libunity_path)?;

    reinstall_modded_app(&temp_apk_path)?;

    info!("Restoring OBB files");
    restore_obb_files(Path::new(APP_OBB_PATH), obb_backups)?;
    std::fs::remove_file(temp_apk_path)?;

    if backed_up_data {
        info!("Restoring player data");
        std::fs::create_dir_all(&APP_DATA_PATH)?;
        std::fs::copy(player_data_backup, player_data_path)?;
    }

    Ok(())
}

fn reinstall_modded_app(temp_apk_path: &Path) -> Result<()> {
    info!("Reinstalling modded app");
    Command::new("pm")
        .args(["uninstall", APK_ID])
        .output()
        .context("Failed to uninstall vanilla APK")?;

    Command::new("pm")
        .args(["install", &temp_apk_path.to_string_lossy()])
        .output()
        .context("Failed to install modded APK")?;

    info!("Granting external storage permission");
    Command::new("appops")
        .args(["set", "--uid", APK_ID, "MANAGE_EXTERNAL_STORAGE", "allow"])
        .output()?;

    Ok(())
}

// Reads the content of the given file path as a Vec
fn read_file_vec(path: impl AsRef<Path>) -> Result<Vec<u8>> {
    let handle = std::fs::File::open(path)?;

    let mut file_content = Vec::with_capacity(handle.metadata()?.len() as usize);
    let mut reader = BufReader::new(handle);
    reader.read_exact(&mut file_content);

    Ok(file_content)
}

// Loads the file from from_path into memory, verifies it matches the checksum of the given diff,
// applies the diff and then outputs it to to_path
fn apply_diff(from_path: &Path,
    to_path: &Path,
    diff: &Diff,
    diffs_path: &Path) -> Result<()> {
    let diff_content = read_file_vec(diffs_path.join(&diff.file_name))
        .context("Diff could not be opened. Was it downloaded")?;

    let patch = qbsdiff::Bspatch::new(&diff_content)
        .context("Diff file was invalid")?;

    let file_content = read_file_vec(from_path)?;

    // Verify the CRC32 hash of the file content.
    info!("Verifying installation is unmodified");
    let before_crc = ZIP_CRC.checksum(&file_content);
    if before_crc != diff.file_crc {
        return Err(anyhow!("File CRC {} did not match expected value of {}. 
            Is the file corrupt, or is the game pirated?", before_crc, diff.file_crc));
    }

    // Carry out the downgrade
    let mut output_handle = OpenOptions::new()
        .truncate(true)
        .create(true)
        .read(true)
        .write(true)
        .open(to_path)?;
    patch.apply(&file_content, &mut output_handle)?;

    // TODO: Verify checksum on the result of downgrading?

    Ok(())

}

// Downloads the deltas needed for downgrading with the given version_diffs.
// The diffs are saved with names matching `diff_name` in the `Diff` struct.
fn download_diffs(to_path: impl AsRef<Path>, version_diffs: &VersionDiffs) -> Result<()> {
    for diff in version_diffs.obb_diffs.iter() {
        info!("Downloading diff for OBB (this may take a long time) {}", diff.file_name);
        download_diff_retry(diff, &to_path)?;
    }

    info!("Downloading diff for APK (this may take a long time)");
    download_diff_retry(&version_diffs.apk_diff, to_path)?;

    Ok(())
}


// Attempts to download the given diff DIFF_DOWNLOAD_ATTEMPTS times, returning an error if the final attempt fails.
fn download_diff_retry(diff: &Diff, to_dir: impl AsRef<Path>) -> Result<()> {
    let mut attempt = 1;
    loop {
        match download_diff(diff, &to_dir) {
            Ok(_) => return Ok(()),
            Err(err) => if attempt == DIFF_DOWNLOAD_ATTEMPTS {
                break Err(err);
            }   else    {
                error!("Failed to download {}: {err}\nTrying again...", diff.diff_name);
            }
        }

        attempt += 1;
    }
}

// Downloads a diff to the given directory, using the file name given in the `Diff` struct.
fn download_diff(diff: &Diff, to_dir: impl AsRef<Path>) -> Result<()> {
    let mut output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(to_dir.as_ref().join(&diff.diff_name))?;

    let (mut resp, length) = external_res::get_diff_reader(diff)?;

    if let Some(length) = length {
        let mut last_progress_update = Instant::now();
        copy_stream_progress(&mut resp, &mut output, &mut |bytes_copied| {
            let now = Instant::now();
            if now.duration_since(last_progress_update).as_secs_f32() > 2.0 {
                last_progress_update = now;
                info!("Progress: {:.2}%", (bytes_copied as f32 / length as f32) * 100.0);
            }
        })?;

    }   else {
        warn!("Diff repository returned no Content-Length, so cannot show download progress");
        std::io::copy(&mut resp, &mut output)?;
    }
    Ok(())
}

fn save_libunity(temp_path: impl AsRef<Path>, app_info: &AppInfo) -> Result<Option<PathBuf>> {
    let mut libunity_stream = match external_res::get_libunity_stream(APK_ID, &app_info.version)? {
        Some(stream) => stream,
        None => return Ok(None) // No libunity for this version
    };

    let libunity_path = temp_path.as_ref().join("libunity.so");
    let mut libunity_handle = OpenOptions::new()
        .truncate(true)
        .write(true)
        .create(true)
        .open(&libunity_path)?;

    std::io::copy(&mut libunity_stream, &mut libunity_handle)?;

    Ok(Some(libunity_path))
}

// Moves the OBB file to a backup location and returns the path that the OBB needs to be restored to
fn save_obbs(obb_dir: &Path, obb_backup_path: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for err_or_stat in std::fs::read_dir(obb_dir)? {
        if let Ok(stat) = err_or_stat {
            let path = stat.path();
            let ext = path.extension();
            // Make sure that we check the extension is OBB: We don't backup DLCs (no extension) since this might cause further issues and they can easily be redownloaded.
            if ext.is_some_and(|ext| ext == "obb") {
                // Rename doesn't work due to different mount points
                std::fs::copy(&path, obb_backup_path.join(path.file_name().unwrap()))?;
                std::fs::remove_file(&path)?;
                
                paths.push(path);
            }
        }
    }

    Ok(paths)
}

// Copies the contents of `obb_backups` back to `restore_dir`, creating it if it doesn't already exist.
fn restore_obb_files(restore_dir: &Path, obb_backups: Vec<PathBuf>) -> Result<()> {
    std::fs::create_dir_all(restore_dir)?;
    for backup_path in obb_backups {
        // Cannot use a `rename` since the mount points are different
        info!("Restoring {:?}", backup_path);
        std::fs::copy(&backup_path, restore_dir.join(backup_path.file_name().unwrap()))?;
        std::fs::remove_file(backup_path);
    }

    Ok(())
}

pub fn get_modloader_path() -> Result<PathBuf> {
    let modloaders_path = format!("/sdcard/ModData/{APK_ID}/Modloader/");

    std::fs::create_dir_all(&modloaders_path)?;
    Ok(PathBuf::from(modloaders_path).join(MODLOADER_NAME))
}

// Copies the modloader to the correct directory on the quest
pub fn install_modloader() -> Result<()> {
    let loader_path = get_modloader_path()?;
    info!("Installing modloader to {loader_path:?}");

    let mut handle = OpenOptions::new()
        .create(true)
        .write(true)
        .open(loader_path)?;
    handle.write_all(MODLOADER)?;
    Ok(())
}

fn patch_apk_in_place(path: impl AsRef<Path>, libunity_path: Option<PathBuf>) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("Failed to open APK");
    
    let mut zip = zip::ZipFile::open(file).unwrap();

    patch_manifest(&mut zip).context("Failed to patch manifest")?;

    let (priv_key, cert) = signing::load_cert_and_priv_key(DEBUG_CERT_PEM);

    zip.delete_file(LIB_MAIN_PATH);
    zip.write_file(LIB_MAIN_PATH, &mut Cursor::new(LIB_MAIN), FileCompression::Deflate)?;
    add_modded_tag(&mut zip, ModTag {
        patcher_name: "ModsBeforeFriday".to_string(),
        patcher_version: Some("0.1.0".to_string()), // TODO: Get this from the frontend maybe?
        modloader_name: "Scotland2".to_string(), // TODO: This should really be Libmainloader because SL2 isn't inside the APK
        modloader_version: None // Temporary, but this field is universally considered to be option so this should be OK.
    })?;

    match libunity_path {
        Some(unity_path) => {
            let mut unity_stream = File::open(unity_path)?;
            zip.write_file(LIB_UNITY_PATH, &mut unity_stream, FileCompression::Deflate)?;
        },
        None => warn!("No unstripped unity added to the APK! This might cause issues later")
    }


    zip.save_and_sign_v2(&cert, &priv_key).context("Failed to save APK")?;

    Ok(())
}

fn add_modded_tag(to: &mut ZipFile<File>, tag: ModTag) -> Result<()> {
    let saved_tag = serde_json::to_vec_pretty(&tag)?;
    to.write_file(MOD_TAG_PATH,
        &mut Cursor::new(saved_tag),
        FileCompression::Deflate
    )?;
    Ok(())
}

pub fn get_modloader_installed(apk: &mut ZipFile<File>) -> Result<Option<ModLoader>> {
    if apk.contains_file(MOD_TAG_PATH) {
        let tag_data = apk.read_file(MOD_TAG_PATH).context("Failed to read mod tag")?;
        let mod_tag: ModTag = match serde_json::from_slice(&tag_data) {
            Ok(tag) => tag,
            Err(err) => {
                warn!("Mod tag was invalid JSON: {err}... Assuming unknown modloader");
                return Ok(Some(ModLoader::Unknown))
            }
        };

        Ok(Some(if mod_tag.modloader_name.eq_ignore_ascii_case("QuestLoader") {
            ModLoader::QuestLoader
        }   else if mod_tag.modloader_name.eq_ignore_ascii_case("Scotland2") {
            // TODO: It's a bit problematic that "Scotland2" is the standard for the contents of modded.json
            // (Since the actual loader inside the APK is libmainloader, which could load any modloader, not just SL2).
            ModLoader::Scotland2
        }   else {
            ModLoader::Unknown
        }))
    }   else if apk.iter_entry_names().any(|entry| entry.contains("modded")) {
        Ok(Some(ModLoader::Unknown))
    }   else {
        Ok(None)
    }
}

fn patch_manifest(zip: &mut ZipFile<File>) -> Result<()> {
    let contents = zip.read_file("AndroidManifest.xml").context("APK had no manifest")?;
    let mut cursor = Cursor::new(contents);
    let mut reader = AxmlReader::new(&mut cursor).context("Failed to read AXML manifest")?;
    let mut data_output = Cursor::new(Vec::new());
    let mut writer = AxmlWriter::new(&mut data_output);

    let manifest = ManifestMod::new()
        .debuggable(true)
        .with_permission("android.permission.MANAGE_EXTERNAL_STORAGE");

    let res_ids = ResourceIds::load()?;
    
    
    manifest.apply_mod(&mut reader, &mut writer, &res_ids).context("Failed to apply mod")?;

    writer.finish().context("Failed to save AXML manifest")?;

    
    cursor.seek(std::io::SeekFrom::Start(0))?;

    zip.delete_file("AndroidManifest.xml");
    zip.write_file(
        "AndroidManifest.xml",
        &mut data_output,
        FileCompression::Deflate
    ).context("Failed to write modified manifest")?;

    Ok(())
}
