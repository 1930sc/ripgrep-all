use crate::adapters::*;
use crate::CachingWriter;
use failure::{format_err, Error};
use path_clean::PathClean;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

// longest compressed conversion output to save in cache
const MAX_DB_BLOB_LEN: usize = 2000000;
const ZSTD_LEVEL: i32 = 12;

pub fn open_cache_db() -> Result<std::sync::Arc<std::sync::RwLock<rkv::Rkv>>, Error> {
    let app_cache = cachedir::CacheDirConfig::new("rga").get_cache_dir()?;

    let db_arc = rkv::Manager::singleton()
        .write()
        .expect("could not write db manager")
        .get_or_create(app_cache.as_path(), |p| {
            let mut builder = rkv::Rkv::environment_builder();
            builder
                .set_flags(rkv::EnvironmentFlags::NO_SYNC | rkv::EnvironmentFlags::WRITE_MAP) // not durable
                .set_map_size(2 * 1024 * 1024 * 1024)
                .set_max_dbs(100);
            rkv::Rkv::from_env(p, builder)
        })
        .expect("could not get/create db");
    Ok(db_arc)
}

pub fn rga_preproc<'a>(
    ai: AdaptInfo<'a>,
    mb_db_arc: Option<std::sync::Arc<std::sync::RwLock<rkv::Rkv>>>,
) -> Result<(), Error> {
    let adapters = adapter_matcher()?;
    let AdaptInfo {
        filepath_hint,
        inp,
        oup,
        line_prefix,
        ..
    } = ai;
    let filename = filepath_hint
        .file_name()
        .ok_or(format_err!("Empty filename"))?;

    eprintln!("abs path: {:?}", filepath_hint);

    /*let mimetype = tree_magic::from_filepath(path).ok_or(lerr(format!(
        "File {} does not exist",
        filename.to_string_lossy()
    )))?;
    println!("mimetype: {:?}", mimetype);*/
    let adapter = adapters(FileMeta {
        // mimetype,
        lossy_filename: filename.to_string_lossy().to_string(),
    });
    match adapter {
        Some(ad) => {
            let meta = ad.metadata();
            eprintln!("adapter: {}", &meta.name);
            let db_name = format!("{}.v{}", meta.name, meta.version);
            if let Some(db_arc) = mb_db_arc {
                let cache_key: Vec<u8> = {
                    let clean_path = filepath_hint.to_owned().clean();
                    eprintln!("clean path: {:?}", clean_path);
                    let meta = std::fs::metadata(&filepath_hint)?;

                    let key = (
                        clean_path,
                        meta.modified().expect("weird OS that can't into mtime"),
                    );
                    eprintln!("cache key: {:?}", key);

                    bincode::serialize(&key).expect("could not serialize path") // key in the cache database
                };
                let db_env = db_arc.read().unwrap();
                let db = db_env
                    .open_single(db_name.as_str(), rkv::store::Options::create())
                    .map_err(|p| format_err!("could not open db store: {:?}", p))?;
                let reader = db_env.read().expect("could not get reader");
                let cached = db
                    .get(&reader, &cache_key)
                    .map_err(|p| format_err!("could not read from db: {:?}", p))?;
                match cached {
                    Some(rkv::Value::Blob(cached)) => {
                        let stdouti = std::io::stdout();
                        zstd::stream::copy_decode(cached, stdouti.lock())?;
                        Ok(())
                    }
                    Some(_) => Err(format_err!("Integrity: value not blob")),
                    None => {
                        let mut compbuf = CachingWriter::new(oup, MAX_DB_BLOB_LEN, ZSTD_LEVEL)?;
                        // start dupe
                        eprintln!("adapting...");
                        ad.adapt(AdaptInfo {
                            line_prefix,
                            filepath_hint,
                            inp,
                            oup: &mut compbuf,
                        })?;
                        // end dupe
                        let compressed = compbuf.finish()?;
                        if let Some(cached) = compressed {
                            eprintln!("compressed len: {}", cached.len());

                            {
                                let mut writer = db_env.write().map_err(|p| {
                                    format_err!("could not open write handle to cache: {:?}", p)
                                })?;
                                db.put(&mut writer, &cache_key, &rkv::Value::Blob(&cached))
                                    .map_err(|p| {
                                        format_err!("could not write to cache: {:?}", p)
                                    })?;
                                writer.commit().unwrap();
                            }
                        }
                        Ok(())
                    }
                }
            } else {
                // todo: duplicate code
                // start dupe
                eprintln!("adapting...");
                ad.adapt(AdaptInfo {
                    line_prefix,
                    filepath_hint,
                    inp,
                    oup,
                })?;
                // end dupe
                Ok(())
            }
        }
        None => {
            let allow_cat = false;
            if allow_cat {
                eprintln!("no adapter for that file, running cat!");
                let stdini = std::io::stdin();
                let mut stdin = stdini.lock();
                let stdouti = std::io::stdout();
                let mut stdout = stdouti.lock();
                std::io::copy(&mut stdin, &mut stdout)?;
                Ok(())
            } else {
                Err(format_err!("No adapter found for file {:?}", filename))
            }
        }
    }
}
