// Copyright 2018 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kvproto::import_sstpb::*;
use uuid::Uuid;

use pd::RegionInfo;

use super::client::*;
use super::common::*;
use super::engine::*;
use super::prepare::*;
use super::stream::*;
use super::{Config, Error, Result};

const MAX_RETRY_TIMES: u64 = 5;
const RETRY_INTERVAL_SECS: u64 = 3;

pub struct ImportJob<Client> {
    tag: String,
    cfg: Config,
    client: Client,
    engine: Arc<Engine>,
    counter: Arc<AtomicUsize>,
}

impl<Client: ImportClient> ImportJob<Client> {
    pub fn new(cfg: Config, client: Client, engine: Engine) -> ImportJob<Client> {
        ImportJob {
            tag: format!("[ImportJob {}]", engine.uuid()),
            cfg,
            client,
            engine: Arc::new(engine),
            counter: Arc::new(AtomicUsize::new(1)),
        }
    }

    pub fn run(&self) -> Result<()> {
        let start = Instant::now();
        info!("{} start", self.tag);

        let job = PrepareJob::new(
            self.cfg.clone(),
            self.client.clone(),
            Arc::clone(&self.engine),
        );
        let ranges = job.run()?;
        let handles = self.run_import_threads(ranges);

        // Join and check results.
        let mut res = Ok(());
        for h in handles {
            if let Err(e) = h.join().unwrap() {
                res = Err(e)
            }
        }

        match res {
            Ok(_) => {
                info!("{} takes {:?}", self.tag, start.elapsed());
                Ok(())
            }
            Err(e) => {
                error!("{}: {:?}", self.tag, e);
                Err(e)
            }
        }
    }

    fn new_import_thread(&self, id: u64, range: RangeInfo) -> JoinHandle<Result<()>> {
        let cfg = self.cfg.clone();
        let client = self.client.clone();
        let engine = Arc::clone(&self.engine);
        let counter = Arc::clone(&self.counter);

        thread::Builder::new()
            .name("import-job".to_owned())
            .spawn(move || {
                let job = SubImportJob::new(id, cfg, range, client, engine, counter);
                job.run()
            })
            .unwrap()
    }

    fn run_import_threads(&self, ranges: Vec<RangeInfo>) -> Vec<JoinHandle<Result<()>>> {
        let mut handles = Vec::new();
        for (i, range) in ranges.into_iter().enumerate() {
            handles.push(self.new_import_thread(i as u64, range));
        }
        handles
    }
}

struct SubImportJob<Client> {
    id: u64,
    tag: String,
    cfg: Config,
    range: RangeInfo,
    client: Arc<Client>,
    engine: Arc<Engine>,
    counter: Arc<AtomicUsize>,
    num_errors: Arc<AtomicUsize>,
    finished_ranges: Arc<Mutex<Vec<Range>>>,
}

impl<Client: ImportClient> SubImportJob<Client> {
    fn new(
        id: u64,
        cfg: Config,
        range: RangeInfo,
        client: Client,
        engine: Arc<Engine>,
        counter: Arc<AtomicUsize>,
    ) -> SubImportJob<Client> {
        SubImportJob {
            id,
            tag: format!("[SubImportJob {}:{}]", engine.uuid(), id),
            cfg,
            range,
            client: Arc::new(client),
            engine,
            counter,
            num_errors: Arc::new(AtomicUsize::new(0)),
            finished_ranges: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn run(&self) -> Result<()> {
        let start = Instant::now();
        info!("{} start {:?}", self.tag, self.range);

        for i in 0..MAX_RETRY_TIMES {
            if i != 0 {
                thread::sleep(Duration::from_secs(RETRY_INTERVAL_SECS));
            }

            let (tx, rx) = mpsc::sync_channel(self.cfg.num_import_sst_jobs);
            let handles = self.run_import_threads(rx);
            if let Err(e) = self.run_import_stream(tx) {
                error!("{} import stream: {:?}", self.tag, e);
                continue;
            }
            for h in handles {
                h.join().unwrap();
            }
            // Check and reset number of errors.
            if self.num_errors.swap(0, Ordering::SeqCst) > 0 {
                continue;
            }

            // Make sure that we don't miss some ranges.
            let mut stream = self.new_import_stream();
            assert!(stream.next().unwrap().is_none());

            let range_count = self.finished_ranges.lock().unwrap().len();
            info!(
                "{} import {} ranges takes {:?}",
                self.tag,
                range_count,
                start.elapsed(),
            );

            return Ok(());
        }

        error!("{} run out of time", self.tag);
        Err(Error::ImportJobFailed(self.tag.clone()))
    }

    fn new_import_stream(&self) -> SSTFileStream<Client> {
        SSTFileStream::new(
            self.cfg.clone(),
            Arc::clone(&self.client),
            Arc::clone(&self.engine),
            self.range.range.clone(),
            self.finished_ranges.lock().unwrap().clone(),
        )
    }

    fn run_import_stream(&self, tx: mpsc::SyncSender<SSTRange>) -> Result<()> {
        let mut stream = self.new_import_stream();
        while let Some(info) = stream.next()? {
            tx.send(info).unwrap();
        }
        Ok(())
    }

    fn run_import_threads(&self, rx: mpsc::Receiver<SSTRange>) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        let rx = Arc::new(Mutex::new(rx));
        for _ in 0..self.cfg.num_import_sst_jobs {
            handles.push(self.new_import_thread(Arc::clone(&rx)));
        }
        handles
    }

    fn new_import_thread(&self, rx: Arc<Mutex<mpsc::Receiver<SSTRange>>>) -> JoinHandle<()> {
        let sub_id = self.id;
        let client = Arc::clone(&self.client);
        let engine = Arc::clone(&self.engine);
        let counter = Arc::clone(&self.counter);
        let num_errors = Arc::clone(&self.num_errors);
        let finished_ranges = Arc::clone(&self.finished_ranges);

        thread::Builder::new()
            .name("import-sst-job".to_owned())
            .spawn(move || {
                'OUTER_LOOP: while let Ok((range, ssts)) = rx.lock().unwrap().recv() {
                    for sst in ssts {
                        let id = counter.fetch_add(1, Ordering::SeqCst);
                        let tag = format!("[ImportSSTJob {}:{}:{}]", engine.uuid(), sub_id, id);
                        let mut job = ImportSSTJob::new(tag, sst, Arc::clone(&client));
                        if job.run().is_err() {
                            num_errors.fetch_add(1, Ordering::SeqCst);
                            continue 'OUTER_LOOP;
                        }
                    }
                    finished_ranges.lock().unwrap().push(range);
                }
            })
            .unwrap()
    }
}

struct ImportSSTJob<Client> {
    tag: String,
    sst: SSTFile,
    client: Arc<Client>,
}

impl<Client: ImportClient> ImportSSTJob<Client> {
    fn new(tag: String, sst: SSTFile, client: Arc<Client>) -> ImportSSTJob<Client> {
        ImportSSTJob { tag, sst, client }
    }

    fn run(&mut self) -> Result<()> {
        let start = Instant::now();
        info!("{} start {:?}", self.tag, self.sst);

        for i in 0..MAX_RETRY_TIMES {
            if i != 0 {
                thread::sleep(Duration::from_secs(RETRY_INTERVAL_SECS));
            }

            let range = self.sst.meta.get_range().clone();
            let mut region = match self.client.get_region(range.get_start()) {
                Ok(region) => if self.sst.inside_region(&region) {
                    region
                } else {
                    warn!("{} outside of {:?}", self.tag, region);
                    return Err(Error::ImportSSTJobFailed(self.tag.clone()));
                },
                Err(e) => {
                    warn!("{}: {:?}", self.tag, e);
                    continue;
                }
            };

            for _ in 0..MAX_RETRY_TIMES {
                match self.import(region) {
                    Ok(_) => {
                        info!("{} takes {:?}", self.tag, start.elapsed());
                        return Ok(());
                    }
                    Err(Error::UpdateRegion(new_region)) => {
                        region = new_region;
                        continue;
                    }
                    Err(_) => break,
                }
            }
        }

        error!("{} run out of time", self.tag);
        Err(Error::ImportSSTJobFailed(self.tag.clone()))
    }

    fn import(&mut self, mut region: RegionInfo) -> Result<()> {
        info!("{} import to {:?}", self.tag, region);

        // Update SST meta for this region.
        {
            let meta = &mut self.sst.meta;
            // Uuid can not be reused, we must generate a new uuid here.
            meta.set_uuid(Uuid::new_v4().as_bytes().to_vec());
            meta.set_region_id(region.get_id());
            meta.set_region_epoch(region.get_region_epoch().clone());
        }

        self.upload(&region)?;

        match self.ingest(&region) {
            Ok(_) => Ok(()),
            Err(Error::NotLeader(new_leader)) => {
                region.leader = new_leader;
                Err(Error::UpdateRegion(region))
            }
            Err(Error::StaleEpoch(new_regions)) => {
                let new_region = new_regions
                    .iter()
                    .find(|&r| self.sst.inside_region(r))
                    .cloned();
                match new_region {
                    Some(new_region) => {
                        let new_leader = region
                            .leader
                            .and_then(|p| find_region_peer(&new_region, p.get_store_id()));
                        Err(Error::UpdateRegion(RegionInfo::new(new_region, new_leader)))
                    }
                    None => {
                        warn!("{} stale epoch {:?}", self.tag, new_regions);
                        Err(Error::StaleEpoch(new_regions))
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    fn upload(&self, region: &RegionInfo) -> Result<()> {
        for peer in region.get_peers() {
            let upload = UploadStream::new(self.sst.meta.clone(), &self.sst.data);
            let store_id = peer.get_store_id();
            match self.client.upload_sst(store_id, upload) {
                Ok(_) => {
                    info!("{} upload to store {}", self.tag, store_id);
                }
                Err(e) => {
                    warn!("{} upload to store {}: {:?}", self.tag, store_id, e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn ingest(&self, region: &RegionInfo) -> Result<()> {
        let ctx = new_context(region);
        let store_id = ctx.get_peer().get_store_id();

        let mut ingest = IngestRequest::new();
        ingest.set_context(ctx);
        ingest.set_sst(self.sst.meta.clone());

        let res = match self.client.ingest_sst(store_id, ingest) {
            Ok(mut resp) => if !resp.has_error() {
                Ok(())
            } else {
                match Error::from(resp.take_error()) {
                    e @ Error::NotLeader(_) | e @ Error::StaleEpoch(_) => return Err(e),
                    e => Err(e),
                }
            },
            Err(e) => Err(e),
        };

        match res {
            Ok(_) => {
                info!("{} ingest to store {}", self.tag, store_id);
                Ok(())
            }
            Err(e) => {
                warn!("{} ingest to store {}: {:?}", self.tag, store_id, e);
                Err(e)
            }
        }
    }
}
