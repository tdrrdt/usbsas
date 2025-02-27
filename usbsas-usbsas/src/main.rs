//! usbsas is the parent of all processes and acts like an orchestrator,
//! spawning and managing every other processes. Only usbsas can send requests
//! to its children. It doesn't do much by itself and he as well waits for
//! requests from the final application.

use log::{debug, error, info, trace, warn};
#[cfg(feature = "log-json")]
use std::sync::{Arc, RwLock};
use std::{
    collections::{HashSet, VecDeque},
    convert::TryFrom,
    io::Write,
};
use thiserror::Error;
use usbsas_comm::{protorequest, protoresponse, Comm};
use usbsas_mass_storage::UsbDevice;
#[cfg(feature = "mock")]
use usbsas_mock::usbdev::MockUsbDev as UsbDev;
use usbsas_process::{UsbsasChild, UsbsasChildSpawner};
use usbsas_proto as proto;
use usbsas_proto::{
    common::*,
    usbsas::{request::Msg, request_copy_start::Destination},
};
#[cfg(not(feature = "mock"))]
use usbsas_usbdev::UsbDev;
use usbsas_utils::READ_FILE_MAX_SIZE;

#[derive(Error, Debug)]
enum Error {
    #[error("io error: {0}")]
    IO(#[from] std::io::Error),
    #[error("{0}")]
    Error(String),
    #[error("analyze error: {0}")]
    Analyze(String),
    #[error("upload error: {0}")]
    Upload(String),
    #[error("int error: {0}")]
    Tryfromint(#[from] std::num::TryFromIntError),
    #[error("privileges: {0}")]
    Privileges(#[from] usbsas_privileges::Error),
    #[error("process error: {0}")]
    Process(#[from] usbsas_process::Error),
    #[error("Bad Request")]
    BadRequest,
    #[error("State error")]
    State,
}
type Result<T> = std::result::Result<T, Error>;

protoresponse!(
    CommUsbsas,
    usbsas,
    end = End[ResponseEnd],
    error = Error[ResponseError],
    id = Id[ResponseId],
    devices = Devices[ResponseDevices],
    opendevice = OpenDevice[ResponseOpenDevice],
    openpartition = OpenPartition[ResponseOpenPartition],
    partitions = Partitions[ResponsePartitions],
    getattr = GetAttr[ResponseGetAttr],
    readdir = ReadDir[ResponseReadDir],
    copystart = CopyStart[ResponseCopyStart],
    copydone = CopyDone[ResponseCopyDone],
    copystatus = CopyStatus[ResponseCopyStatus],
    copystatusdone = CopyStatusDone[ResponseCopyStatusDone],
    analyzestatus = AnalyzeStatus[ResponseAnalyzeStatus],
    analyzedone = AnalyzeDone[ResponseAnalyzeDone],
    finalcopystatus = FinalCopyStatus[ResponseFinalCopyStatus],
    finalcopystatusdone = FinalCopyStatusDone[ResponseFinalCopyStatusDone],
    notenoughspace = NotEnoughSpace[ResponseNotEnoughSpace],
    nothingtocopy = NothingToCopy[ResponseNothingToCopy],
    wipe = Wipe[ResponseWipe],
    imgdisk = ImgDisk[ResponseImgDisk],
    postcopycmd = PostCopyCmd[ResponsePostCopyCmd]
);

protorequest!(
    CommFilter,
    filter,
    filterpaths = FilterPaths[RequestFilterPaths, ResponseFilterPaths],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommIdentificator,
    identificator,
    id = Id[RequestId, ResponseId],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommFs2dev,
    fs2dev,
    size = DevSize[RequestDevSize, ResponseDevSize],
    startcopy = StartCopy[RequestStartCopy, ResponseStartCopy],
    wipe = Wipe[RequestWipe, ResponseWipe],
    loadbitvec = LoadBitVec[RequestLoadBitVec, ResponseLoadBitVec],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommUsbdev,
    usbdev,
    devices = Devices[RequestDevices, ResponseDevices],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommFiles,
    files,
    opendevice = OpenDevice[RequestOpenDevice, ResponseOpenDevice],
    partitions = Partitions[RequestPartitions, ResponsePartitions],
    openpartition = OpenPartition[RequestOpenPartition, ResponseOpenPartition],
    getattr = GetAttr[RequestGetAttr, ResponseGetAttr],
    readdir = ReadDir[RequestReadDir, ResponseReadDir],
    readfile = ReadFile[RequestReadFile, ResponseReadFile],
    readsectors = ReadSectors[RequestReadSectors, ResponseReadSectors],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommWritefs,
    writefs,
    setfsinfos = SetFsInfos[RequestSetFsInfos, ResponseSetFsInfos],
    newfile = NewFile[RequestNewFile, ResponseNewFile],
    writefile = WriteFile[RequestWriteFile, ResponseWriteFile],
    endfile = EndFile[RequestEndFile, ResponseEndFile],
    close = Close[RequestClose, ResponseClose],
    bitvec = BitVec[RequestBitVec, ResponseBitVec],
    imgdisk = ImgDisk[RequestImgDisk, ResponseImgDisk],
    writedata = WriteData[RequestWriteData, ResponseWriteData],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommWritetar,
    writetar,
    newfile = NewFile[RequestNewFile, ResponseNewFile],
    writefile = WriteFile[RequestWriteFile, ResponseWriteFile],
    endfile = EndFile[RequestEndFile, ResponseEndFile],
    close = Close[RequestClose, ResponseClose],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommUploader,
    uploader,
    upload = Upload[RequestUpload, ResponseUpload],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommCmdExec,
    cmdexec,
    exec = Exec[RequestExec, ResponseExec],
    postcopyexec = PostCopyExec[RequestPostCopyExec, ResponsePostCopyExec],
    end = End[RequestEnd, ResponseEnd]
);

protorequest!(
    CommAnalyzer,
    analyzer,
    analyze = Analyze[RequestAnalyze, ResponseAnalyze],
    end = End[RequestEnd, ResponseEnd]
);

enum State {
    Init(InitState),
    DevOpened(DevOpenedState),
    PartitionOpened(PartitionOpenedState),
    CopyFiles(CopyFilesState),
    WriteFiles(WriteFilesState),
    UploadOrCmd(UploadOrCmdState),
    TransferDone(TransferDoneState),
    Wipe(WipeState),
    ImgDisk(ImgDiskState),
    WaitEnd(WaitEndState),
    End,
}

impl State {
    fn run(self, comm: &mut Comm<proto::usbsas::Request>, children: &mut Children) -> Result<Self> {
        match self {
            State::Init(s) => s.run(comm, children),
            State::DevOpened(s) => s.run(comm, children),
            State::PartitionOpened(s) => s.run(comm, children),
            State::CopyFiles(s) => s.run(comm, children),
            State::WriteFiles(s) => s.run(comm, children),
            State::UploadOrCmd(s) => s.run(comm, children),
            State::TransferDone(s) => s.run(comm, children),
            State::Wipe(s) => s.run(comm, children),
            State::ImgDisk(s) => s.run(comm, children),
            State::WaitEnd(s) => s.run(comm, children),
            State::End => Err(Error::State),
        }
    }
}

struct InitState {}

impl InitState {
    fn run(
        mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        debug!("started usbsas");
        let mut id: Option<String> = None;
        loop {
            let req: proto::usbsas::Request = comm.recv()?;
            let res = match req.msg.ok_or(Error::BadRequest)? {
                Msg::Id(_) => children.id(comm, &mut id),
                Msg::Devices(_) => self.devices(comm, children),
                Msg::OpenDevice(req) => {
                    match self.open_device(comm, children, req.device.ok_or(Error::BadRequest)?) {
                        Ok(device) => return Ok(State::DevOpened(DevOpenedState { device, id })),
                        Err(err) => Err(err),
                    }
                }
                Msg::Wipe(req) => {
                    return Ok(State::Wipe(WipeState {
                        busnum: req.busnum as u64,
                        devnum: req.devnum as u64,
                        quick: req.quick,
                        fstype: req.fstype,
                    }))
                }
                Msg::ImgDisk(req) => {
                    match self.open_device(comm, children, req.device.ok_or(Error::BadRequest)?) {
                        Ok(device) => return Ok(State::ImgDisk(ImgDiskState { device })),
                        Err(err) => Err(err),
                    }
                }
                Msg::End(_) => {
                    children.end_wait_all(comm)?;
                    break;
                }
                _ => Err(Error::BadRequest),
            };
            if let Err(err) = res {
                error!("{}", err);
                comm.error(proto::usbsas::ResponseError {
                    err: format!("{}", err),
                })?;
            }
        }
        Ok(State::End)
    }

    fn devices(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<()> {
        trace!("req devices");
        comm.devices(proto::usbsas::ResponseDevices {
            devices: children
                .usbdev
                .comm
                .devices(proto::usbdev::RequestDevices {})?
                .devices,
        })?;
        Ok(())
    }

    fn open_device(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
        dev_req: Device,
    ) -> Result<UsbDevice> {
        trace!("req opendevice");
        let device = children
            .scsi2files
            .comm
            .opendevice(proto::files::RequestOpenDevice {
                busnum: dev_req.busnum,
                devnum: dev_req.devnum,
            })?;
        comm.opendevice(proto::usbsas::ResponseOpenDevice {
            sector_size: device.block_size,
            dev_size: device.dev_size,
        })?;
        Ok(UsbDevice {
            busnum: dev_req.busnum,
            devnum: dev_req.devnum,
            vendorid: dev_req.vendorid,
            productid: dev_req.productid,
            manufacturer: dev_req.manufacturer,
            serial: dev_req.serial,
            description: dev_req.description,
            sector_size: u32::try_from(device.block_size)?,
            dev_size: device.dev_size,
        })
    }
}

struct DevOpenedState {
    device: UsbDevice,
    id: Option<String>,
}

impl DevOpenedState {
    fn run(
        mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        loop {
            let req: proto::usbsas::Request = comm.recv()?;
            let res = match req.msg.ok_or(Error::BadRequest)? {
                Msg::Id(_) => children.id(comm, &mut self.id),
                Msg::Partitions(_) => self.partitions(comm, children),
                Msg::OpenPartition(req) => match self.open_partition(comm, children, req.index) {
                    Ok(_) => {
                        return Ok(State::PartitionOpened(PartitionOpenedState {
                            device: self.device,
                            id: self.id,
                        }))
                    }
                    Err(err) => {
                        error!("{}", err);
                        Err(err)
                    }
                },
                Msg::End(_) => {
                    children.end_wait_all(comm)?;
                    break;
                }
                _ => Err(Error::BadRequest),
            };
            if let Err(err) = res {
                error!("{}", err);
                comm.error(proto::usbsas::ResponseError {
                    err: format!("{}", err),
                })?;
            }
        }
        Ok(State::End)
    }

    fn partitions(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<()> {
        trace!("req partitions");
        comm.partitions(proto::usbsas::ResponsePartitions {
            partitions: children
                .scsi2files
                .comm
                .partitions(proto::files::RequestPartitions {})?
                .partitions,
        })?;
        Ok(())
    }

    fn open_partition(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
        index: u32,
    ) -> Result<()> {
        trace!("req open partition");
        children
            .scsi2files
            .comm
            .openpartition(proto::files::RequestOpenPartition { index })?;
        comm.openpartition(proto::usbsas::ResponseOpenPartition {})?;
        Ok(())
    }
}

struct PartitionOpenedState {
    device: UsbDevice,
    id: Option<String>,
}

impl PartitionOpenedState {
    fn run(
        mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        loop {
            let req: proto::usbsas::Request = comm.recv()?;
            let res = match req.msg.ok_or(Error::BadRequest)? {
                Msg::Id(_) => children.id(comm, &mut self.id),
                Msg::GetAttr(req) => self.get_attr(comm, children, req.path),
                Msg::ReadDir(req) => self.read_dir(comm, children, req.path),
                Msg::CopyStart(req) => {
                    if let Some(id) = self.id {
                        return Ok(State::CopyFiles(CopyFilesState {
                            device: self.device,
                            id,
                            selected: req.selected,
                            destination: req.destination.ok_or(Error::BadRequest)?,
                        }));
                    }
                    error!("empty id");
                    Err(Error::BadRequest)
                }
                Msg::End(_) => {
                    children.end_wait_all(comm)?;
                    break;
                }
                _ => Err(Error::BadRequest),
            };
            if let Err(err) = res {
                error!("{}", err);
                comm.error(proto::usbsas::ResponseError {
                    err: format!("{}", err),
                })?;
            }
        }
        Ok(State::End)
    }

    fn get_attr(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
        path: String,
    ) -> Result<()> {
        trace!("req get attr: {}", &path);
        let attrs = children
            .scsi2files
            .comm
            .getattr(proto::files::RequestGetAttr { path })?;
        comm.getattr(proto::usbsas::ResponseGetAttr {
            ftype: attrs.ftype,
            size: attrs.size,
            timestamp: attrs.timestamp,
        })?;
        Ok(())
    }

    fn read_dir(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
        path: String,
    ) -> Result<()> {
        trace!("req read dir attrs: {}", &path);
        comm.readdir(proto::usbsas::ResponseReadDir {
            filesinfo: children
                .scsi2files
                .comm
                .readdir(proto::files::RequestReadDir { path })?
                .filesinfo,
        })?;
        Ok(())
    }
}

struct CopyFilesState {
    destination: Destination,
    device: UsbDevice,
    id: String,
    selected: Vec<String>,
}

impl CopyFilesState {
    fn run(
        mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        trace!("req copy");
        info!("Usbsas transfer for user: {}", self.id);

        let mut errors = vec![];
        let mut all_directories = vec![];
        let mut all_files = vec![];
        let total_files_size = self.selected_to_files_list(
            children,
            &mut errors,
            &mut all_files,
            &mut all_directories,
        )?;
        let mut filtered: Vec<String> = Vec::new();

        let all_files_filtered = self.filter_files(children, all_files, &mut filtered)?;
        let all_directories_filtered =
            self.filter_files(children, all_directories, &mut filtered)?;

        let mut all_entries_filtered = vec![];
        all_entries_filtered.append(&mut all_directories_filtered.clone());
        all_entries_filtered.append(&mut all_files_filtered.clone());

        // Abort if no files passed name filtering
        if all_entries_filtered.is_empty() {
            comm.nothingtocopy(proto::usbsas::ResponseNothingToCopy {
                rejected_filter: filtered,
                rejected_dirty: vec![],
            })?;
            warn!("Aborting copy, no files survived filter");
            return Ok(State::WaitEnd(WaitEndState {}));
        }

        // max_file_size is 4GB if we're writing a FAT fs, None otherwise
        let max_file_size = match self.destination {
            Destination::Usb(ref usb) => {
                // Unlock fs2dev to get dev_size
                children.fs2dev.comm.write_all(
                    &(((u64::from(usb.devnum)) << 32) | (u64::from(usb.busnum))).to_ne_bytes(),
                )?;
                children.fs2dev.locked = false;
                let dev_size = children
                    .fs2dev
                    .comm
                    .size(proto::fs2dev::RequestDevSize {})?
                    .size;
                // Check dest dev is large enough
                // XXX try to be more precise about this
                if total_files_size > (dev_size * 98 / 100) {
                    comm.notenoughspace(proto::usbsas::ResponseNotEnoughSpace {
                        max_size: dev_size,
                    })?;
                    error!("Aborting, dest dev too small");
                    return Ok(State::WaitEnd(WaitEndState {}));
                }
                match OutFsType::from_i32(usb.fstype)
                    .ok_or_else(|| Error::Error("bad fstype".into()))?
                {
                    OutFsType::Fat => Some(0xFFFF_FFFF),
                    _ => None,
                }
            }
            Destination::Net(_) | Destination::Cmd(_) => None,
        };

        // Unlock files2tar
        children.files2tar.comm.write_all(&[0_u8])?;
        children.files2tar.locked = false;

        comm.copystart(proto::usbsas::ResponseCopyStart { total_files_size })?;

        self.tar_src_files(
            comm,
            children,
            &all_entries_filtered,
            &mut errors,
            max_file_size,
        )?;

        match self.destination {
            Destination::Usb(usb) => {
                children.tar2files.comm.write_all(&[1_u8])?;
                children.tar2files.locked = false;
                Ok(State::WriteFiles(WriteFilesState {
                    directories: all_directories_filtered,
                    dirty: Vec::new(),
                    errors,
                    files: all_files_filtered,
                    filtered,
                    id: self.id,
                    usb,
                }))
            }
            Destination::Net(_) | Destination::Cmd(_) => {
                children.tar2files.comm.write_all(&[0_u8])?;
                children.tar2files.locked = false;
                Ok(State::UploadOrCmd(UploadOrCmdState {
                    errors,
                    filtered,
                    id: self.id,
                    destination: self.destination,
                }))
            }
        }
    }

    /// Expand tree of selected files and directories and compute total files size
    fn selected_to_files_list(
        &mut self,
        children: &mut Children,
        errors: &mut Vec<String>,
        files: &mut Vec<String>,
        directories: &mut Vec<String>,
    ) -> Result<u64> {
        let mut total_size: u64 = 0;
        let mut todo = VecDeque::from(self.selected.to_vec());
        let mut all_entries = HashSet::new();
        while let Some(entry) = todo.pop_front() {
            // First add parent(s) of file if not selected
            let mut parts = entry.trim_start_matches('/').split('/');
            // Remove last (file basename)
            let _ = parts.next_back();
            let mut parent = String::from("");
            for dir in parts {
                parent.push('/');
                parent.push_str(dir);
                if !directories.contains(&parent) {
                    directories.push(parent.clone());
                }
            }
            let rep = match children
                .scsi2files
                .comm
                .getattr(proto::files::RequestGetAttr {
                    path: entry.clone(),
                }) {
                Ok(rep) => rep,
                Err(_) => {
                    errors.push(entry);
                    continue;
                }
            };
            match FileType::from_i32(rep.ftype) {
                Some(FileType::Regular) => {
                    if !all_entries.contains(&entry) {
                        files.push(entry.clone());
                        all_entries.insert(entry);
                        total_size += rep.size;
                    }
                }
                Some(FileType::Directory) => {
                    if !all_entries.contains(&entry) {
                        directories.push(entry.clone());
                        all_entries.insert(entry.clone());
                    }
                    let rep = children
                        .scsi2files
                        .comm
                        .readdir(proto::files::RequestReadDir { path: entry })?;
                    for file in rep.filesinfo.iter() {
                        todo.push_back(file.path.clone());
                    }
                }
                _ => errors.push(entry),
            }
        }
        Ok(total_size)
    }

    fn filter_files(
        &mut self,
        children: &mut Children,
        files: Vec<String>,
        filtered: &mut Vec<String>,
    ) -> Result<Vec<String>> {
        trace!("filter files");
        let mut filtered_files: Vec<String> = Vec::new();
        let files_count = files.len();
        let rep = children
            .filter
            .comm
            .filterpaths(proto::filter::RequestFilterPaths {
                path: files.to_vec(),
            })?;
        if rep.results.len() != files_count {
            return Err(Error::Error("filter error".to_string()));
        }
        for (i, f) in files.iter().enumerate().take(files_count) {
            if rep.results[i] == proto::filter::FilterResult::PathOk as i32 {
                filtered_files.push(f.clone());
            } else {
                filtered.push(f.clone());
            }
        }
        Ok(filtered_files)
    }

    fn tar_src_files(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
        selected: &[String],
        errors: &mut Vec<String>,
        max_file_size: Option<u64>,
    ) -> Result<()> {
        trace!("tar src files");
        for path in selected {
            if let Err(err) = self.file_to_tar(comm, children, path, max_file_size) {
                error!("Couldn't copy file {}: {}", &path, err);
                errors.push(path.clone());
            };
        }
        children
            .files2tar
            .comm
            .close(proto::writetar::RequestClose {
                id: self.id.clone(),
                vendorid: self.device.vendorid,
                productid: self.device.productid,
                manufacturer: self.device.manufacturer.clone(),
                serial: self.device.serial.clone(),
                description: self.device.description.clone(),
            })?;
        comm.copystatusdone(proto::usbsas::ResponseCopyStatusDone {})?;
        Ok(())
    }

    fn file_to_tar(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
        path: &str,
        max_file_size: Option<u64>,
    ) -> Result<()> {
        let mut attrs = children
            .scsi2files
            .comm
            .getattr(proto::files::RequestGetAttr { path: path.into() })?;

        if let Some(max_size) = max_file_size {
            if attrs.size > max_size {
                error!(
                    "File '{}' is larger ({}B) than max size ({}B)",
                    &path, attrs.size, max_size
                );
                return Err(Error::Error("file too large".into()));
            }
        }

        // Some FS (like ext4) have a directory size != 0, fix it here for the tar archive.
        if let Some(FileType::Directory) = FileType::from_i32(attrs.ftype) {
            attrs.size = 0;
        }

        children
            .files2tar
            .comm
            .newfile(proto::writetar::RequestNewFile {
                path: path.to_string(),
                size: attrs.size,
                ftype: attrs.ftype,
                timestamp: attrs.timestamp,
            })?;

        let mut offset: u64 = 0;
        while attrs.size > 0 {
            let size_todo = if attrs.size < READ_FILE_MAX_SIZE {
                attrs.size
            } else {
                READ_FILE_MAX_SIZE
            };
            let rep = children
                .scsi2files
                .comm
                .readfile(proto::files::RequestReadFile {
                    path: path.to_string(),
                    offset,
                    size: size_todo,
                })?;
            children
                .files2tar
                .comm
                .writefile(proto::writetar::RequestWriteFile {
                    path: path.to_string(),
                    offset,
                    data: rep.data,
                })?;
            offset += size_todo;
            attrs.size -= size_todo;
            comm.copystatus(proto::usbsas::ResponseCopyStatus {
                current_size: size_todo,
            })?;
        }

        children
            .files2tar
            .comm
            .endfile(proto::writetar::RequestEndFile {
                path: path.to_string(),
            })?;

        Ok(())
    }
}

struct WriteFilesState {
    directories: Vec<String>,
    dirty: Vec<String>,
    errors: Vec<String>,
    files: Vec<String>,
    filtered: Vec<String>,
    id: String,
    usb: proto::usbsas::DestUsb,
}

impl WriteFilesState {
    fn run(
        mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        self.analyze_files(comm, children)?;

        // Abort if no files survived antivirus
        if self.files.is_empty() {
            comm.nothingtocopy(proto::usbsas::ResponseNothingToCopy {
                rejected_filter: self.filtered,
                rejected_dirty: self.dirty,
            })?;
            warn!("Aborting copy, no files survived antivirus");
            return Ok(State::WaitEnd(WaitEndState {}));
        }

        self.init_fs(children)?;

        trace!("copy usb");

        // Create directory tree
        for dir in &self.directories {
            let timestamp = children
                .tar2files
                .comm
                .getattr(proto::files::RequestGetAttr { path: dir.clone() })?
                .timestamp;
            children
                .files2fs
                .comm
                .newfile(proto::writefs::RequestNewFile {
                    path: dir.to_string(),
                    size: 0,
                    ftype: FileType::Directory.into(),
                    timestamp,
                })?;
        }

        // Copy files
        for path in &self.files {
            let attrs = match children
                .tar2files
                .comm
                .getattr(proto::files::RequestGetAttr { path: path.clone() })
            {
                Ok(rep) => rep,
                Err(err) => {
                    error!("{}", err);
                    self.errors.push(path.clone());
                    continue;
                }
            };

            match self.write_file(
                comm,
                children,
                path,
                attrs.size,
                attrs.ftype,
                attrs.timestamp,
            ) {
                Ok(_) => (),
                Err(err) => {
                    warn!("didn't copy file {}: {}", path, err);
                    self.errors.push(path.clone());
                }
            }
        }

        children
            .files2fs
            .comm
            .close(proto::writefs::RequestClose {})?;
        comm.copystatusdone(proto::usbsas::ResponseCopyStatusDone {})?;

        children.forward_bitvec()?;
        match self.write_fs(comm, children) {
            Ok(()) => {
                comm.copydone(proto::usbsas::ResponseCopyDone {
                    error_path: self.errors,
                    filtered_path: self.filtered,
                    dirty_path: self.dirty,
                })?;
                info!("USB TRANSFER DONE for user {}", self.id);
            }
            Err(err) => {
                comm.error(proto::usbsas::ResponseError {
                    err: format!("err writing fs: {}", err),
                })?;
                error!("USB TRANSFER FAILED for user {}", self.id);
            }
        }

        Ok(State::TransferDone(TransferDoneState {}))
    }

    fn init_fs(&mut self, children: &mut Children) -> Result<()> {
        trace!("init fs");
        let dev_size = children
            .fs2dev
            .comm
            .size(proto::fs2dev::RequestDevSize {})?
            .size;
        children
            .files2fs
            .comm
            .setfsinfos(proto::writefs::RequestSetFsInfos {
                dev_size,
                fstype: self.usb.fstype,
            })?;
        Ok(())
    }

    fn analyze_files(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<()> {
        trace!("analyzing files");
        use proto::analyzer::response::Msg;
        if let Some(ref mut analyzer) = children.analyzer {
            analyzer.comm.send(proto::analyzer::Request {
                msg: Some(proto::analyzer::request::Msg::Analyze(
                    proto::analyzer::RequestAnalyze {
                        id: self.id.to_string(),
                    },
                )),
            })?;

            loop {
                let rep: proto::analyzer::Response = analyzer.comm.recv()?;
                match rep.msg.ok_or(Error::BadRequest)? {
                    Msg::Analyze(res) => {
                        debug!(
                            "Analyzer status: clean: {:#?}, dirty: {:#?}",
                            &res.clean, &res.dirty
                        );
                        self.files
                            .retain(|x| res.clean.contains(&x.trim_start_matches('/').to_string()));
                        res.dirty
                            .iter()
                            .for_each(|p| self.dirty.push(format!("/{}", p)));
                        comm.analyzedone(proto::usbsas::ResponseAnalyzeDone {})?;
                        return Ok(());
                    }
                    Msg::UploadStatus(status) => {
                        comm.analyzestatus(proto::usbsas::ResponseAnalyzeStatus {
                            current_size: status.current_size,
                            total_size: status.total_size,
                        })?;
                        continue;
                    }
                    Msg::Error(err) => {
                        error!("{}", err.err);
                        return Err(Error::Analyze(err.err));
                    }
                    _ => return Err(Error::Analyze("Unexpected response".into())),
                }
            }
        };
        Ok(())
    }

    fn write_file(
        &self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
        path: &str,
        size: u64,
        ftype: i32,
        timestamp: i64,
    ) -> Result<()> {
        children
            .files2fs
            .comm
            .newfile(proto::writefs::RequestNewFile {
                path: path.to_string(),
                size,
                ftype,
                timestamp,
            })?;
        let mut size = size;
        let mut offset: u64 = 0;
        while size > 0 {
            let size_todo = if size < READ_FILE_MAX_SIZE {
                size
            } else {
                READ_FILE_MAX_SIZE
            };
            let rep = match children
                .tar2files
                .comm
                .readfile(proto::files::RequestReadFile {
                    path: path.to_string(),
                    offset,
                    size: size_todo,
                }) {
                Ok(rep) => rep,
                Err(err) => {
                    return Err(Error::Error(format!("{}", err)));
                }
            };
            children
                .files2fs
                .comm
                .writefile(proto::writefs::RequestWriteFile {
                    path: path.to_string(),
                    offset,
                    data: rep.data,
                })?;
            offset += size_todo;
            size -= size_todo;
            comm.copystatus(proto::usbsas::ResponseCopyStatus {
                current_size: size_todo,
            })?;
        }
        children
            .files2fs
            .comm
            .endfile(proto::writefs::RequestEndFile {
                path: path.to_string(),
            })?;
        Ok(())
    }

    fn write_fs(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<()> {
        use proto::fs2dev::response::Msg;
        children
            .fs2dev
            .comm
            .startcopy(proto::fs2dev::RequestStartCopy {})?;
        loop {
            let rep: proto::fs2dev::Response = children.fs2dev.comm.recv()?;
            match rep.msg.ok_or(Error::BadRequest)? {
                Msg::CopyStatus(status) => {
                    comm.finalcopystatus(proto::usbsas::ResponseFinalCopyStatus {
                        current_size: status.current_size,
                        total_size: status.total_size,
                    })?;
                }
                Msg::CopyStatusDone(_) => {
                    comm.finalcopystatusdone(proto::usbsas::ResponseFinalCopyStatusDone {})?;
                    break;
                }
                Msg::Error(msg) => return Err(Error::Error(msg.err)),
                _ => return Err(Error::Error("error writing fs".into())),
            }
        }
        Ok(())
    }
}

struct UploadOrCmdState {
    destination: Destination,
    errors: Vec<String>,
    filtered: Vec<String>,
    id: String,
}

impl UploadOrCmdState {
    fn run(
        mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        match self.destination {
            Destination::Usb(_) => unreachable!("already handled"),
            Destination::Net(_) => self.upload_files(comm, children)?,
            Destination::Cmd(_) => {
                trace!("exec cmd");
                children.cmdexec.comm.exec(proto::cmdexec::RequestExec {})?;
            }
        }

        // Unlock fs2dev so it can exit
        children.fs2dev.comm.write_all(&(0_u64).to_ne_bytes())?;
        children.fs2dev.locked = false;

        comm.finalcopystatusdone(proto::usbsas::ResponseFinalCopyStatusDone {})?;
        comm.copydone(proto::usbsas::ResponseCopyDone {
            error_path: self.errors,
            filtered_path: self.filtered,
            dirty_path: Vec::new(),
        })?;

        info!("NET TRANSFER DONE for user {}", self.id);
        Ok(State::TransferDone(TransferDoneState {}))
    }

    fn upload_files(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<()> {
        use proto::uploader::response::Msg;
        trace!("upload bundle");
        children.uploader.comm.send(proto::uploader::Request {
            msg: Some(proto::uploader::request::Msg::Upload(
                proto::uploader::RequestUpload {
                    id: self.id.clone(),
                },
            )),
        })?;

        loop {
            let rep: proto::uploader::Response = children.uploader.comm.recv()?;
            match rep.msg.ok_or(Error::BadRequest)? {
                Msg::UploadStatus(status) => {
                    comm.finalcopystatus(proto::usbsas::ResponseFinalCopyStatus {
                        current_size: status.current_size,
                        total_size: status.total_size,
                    })?;
                }
                Msg::Upload(_) => {
                    debug!("files uploaded");
                    break;
                }
                Msg::Error(err) => {
                    error!("Upload error: {:?}", err);
                    return Err(Error::Upload(err.err));
                }
                _ => {
                    error!("bad resp");
                    return Err(Error::BadRequest);
                }
            }
        }

        Ok(())
    }
}

struct WipeState {
    busnum: u64,
    devnum: u64,
    quick: bool,
    fstype: i32,
}

impl WipeState {
    fn run(
        self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        use proto::fs2dev::response::Msg;
        trace!("req wipe");

        // Unlock fs2dev
        children
            .fs2dev
            .comm
            .write_all(&((self.devnum << 32) | self.busnum).to_ne_bytes())?;
        children.fs2dev.locked = false;

        if !self.quick {
            trace!("secure wipe");
            children.fs2dev.comm.wipe(proto::fs2dev::RequestWipe {})?;
            loop {
                let rep: proto::fs2dev::Response = children.fs2dev.comm.recv()?;
                match rep.msg.ok_or(Error::BadRequest)? {
                    Msg::CopyStatus(status) => {
                        comm.finalcopystatus(proto::usbsas::ResponseFinalCopyStatus {
                            current_size: status.current_size,
                            total_size: status.total_size,
                        })?
                    }
                    Msg::CopyStatusDone(_) => break,
                    _ => {
                        return Err(Error::Error("fs2dev err while wiping".into()));
                    }
                }
            }
        }

        comm.finalcopystatusdone(proto::usbsas::ResponseFinalCopyStatusDone {})?;

        let dev_size = children
            .fs2dev
            .comm
            .size(proto::fs2dev::RequestDevSize {})?
            .size;
        children
            .files2fs
            .comm
            .setfsinfos(proto::writefs::RequestSetFsInfos {
                dev_size,
                fstype: self.fstype,
            })?;
        children
            .files2fs
            .comm
            .close(proto::writefs::RequestClose {})?;
        children.forward_bitvec()?;

        children
            .fs2dev
            .comm
            .startcopy(proto::fs2dev::RequestStartCopy {})?;
        loop {
            let rep: proto::fs2dev::Response = children.fs2dev.comm.recv()?;
            match rep.msg.ok_or(Error::BadRequest)? {
                Msg::CopyStatus(status) => {
                    comm.finalcopystatus(proto::usbsas::ResponseFinalCopyStatus {
                        current_size: status.current_size,
                        total_size: status.total_size,
                    })?;
                }
                Msg::CopyStatusDone(_) => {
                    comm.wipe(proto::usbsas::ResponseWipe {})?;
                    break;
                }
                _ => {
                    error!("bad response");
                    comm.error(proto::usbsas::ResponseError {
                        err: "bad response received from fs2dev".into(),
                    })?;
                    break;
                }
            }
        }

        info!(
            "WIPE DONE (bus/devnum: {}/{} - quick: {})",
            self.busnum, self.devnum, self.quick
        );
        Ok(State::WaitEnd(WaitEndState {}))
    }
}

struct ImgDiskState {
    device: UsbDevice,
}

impl ImgDiskState {
    fn run(
        self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        trace!("Image disk");
        self.image_disk(comm, children)?;
        comm.imgdisk(proto::usbsas::ResponseImgDisk {})?;
        Ok(State::WaitEnd(WaitEndState {}))
    }

    fn image_disk(
        &self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<()> {
        children
            .files2fs
            .comm
            .imgdisk(proto::writefs::RequestImgDisk {})?;

        let mut todo = self.device.dev_size as u64;
        let mut sector_count: u64 = READ_FILE_MAX_SIZE / self.device.sector_size as u64;
        let mut offset = 0;

        while todo != 0 {
            if todo < READ_FILE_MAX_SIZE {
                sector_count = todo / self.device.sector_size as u64;
            }
            let rep = children
                .scsi2files
                .comm
                .readsectors(proto::files::RequestReadSectors {
                    offset,
                    count: sector_count,
                })?;
            children
                .files2fs
                .comm
                .writedata(proto::writefs::RequestWriteData { data: rep.data })?;
            offset += sector_count;
            todo -= sector_count * self.device.sector_size as u64;
            comm.finalcopystatus(proto::usbsas::ResponseFinalCopyStatus {
                current_size: offset * self.device.sector_size as u64,
                total_size: self.device.dev_size,
            })?;
        }
        info!("DISK IMAGE DONE");
        Ok(())
    }
}

struct TransferDoneState {}

impl TransferDoneState {
    fn run(
        self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        let req: proto::usbsas::Request = comm.recv()?;
        match req.msg.ok_or(Error::BadRequest)? {
            Msg::End(_) => {
                children.end_wait_all(comm)?;
                return Ok(State::End);
            }
            Msg::PostCopyCmd(req) => {
                trace!("post copy cmd");
                match children
                    .cmdexec
                    .comm
                    .postcopyexec(proto::cmdexec::RequestPostCopyExec {
                        outfiletype: req.outfiletype,
                    }) {
                    Ok(_) => {
                        comm.postcopycmd(proto::usbsas::ResponsePostCopyCmd {})?;
                    }
                    Err(err) => {
                        error!("post copy cmd error: {}", err);
                        comm.error(proto::usbsas::ResponseError {
                            err: format!("{}", err),
                        })?;
                    }
                }
            }
            _ => {
                error!("bad req");
                comm.error(proto::usbsas::ResponseError {
                    err: "bad req".into(),
                })?;
            }
        }
        Ok(State::WaitEnd(WaitEndState {}))
    }
}

struct WaitEndState {}

impl WaitEndState {
    fn run(
        self,
        comm: &mut Comm<proto::usbsas::Request>,
        children: &mut Children,
    ) -> Result<State> {
        loop {
            let req: proto::usbsas::Request = comm.recv()?;
            match req.msg.ok_or(Error::BadRequest)? {
                Msg::End(_) => {
                    children.end_wait_all(comm)?;
                    break;
                }
                _ => {
                    error!("bad req");
                    comm.error(proto::usbsas::ResponseError {
                        err: "bad req".into(),
                    })?;
                    continue;
                }
            }
        }
        Ok(State::End)
    }
}

struct Children {
    analyzer: Option<UsbsasChild<proto::analyzer::Request>>,
    identificator: UsbsasChild<proto::identificator::Request>,
    cmdexec: UsbsasChild<proto::cmdexec::Request>,
    files2fs: UsbsasChild<proto::writefs::Request>,
    files2tar: UsbsasChild<proto::writetar::Request>,
    filter: UsbsasChild<proto::filter::Request>,
    fs2dev: UsbsasChild<proto::fs2dev::Request>,
    scsi2files: UsbsasChild<proto::files::Request>,
    tar2files: UsbsasChild<proto::files::Request>,
    uploader: UsbsasChild<proto::uploader::Request>,
    usbdev: UsbsasChild<proto::usbdev::Request>,
}

// Functions shared by multiple states are implementend on this struct.
impl Children {
    fn id(
        &mut self,
        comm: &mut Comm<proto::usbsas::Request>,
        id: &mut Option<String>,
    ) -> Result<()> {
        trace!("req id");
        let newid = self
            .identificator
            .comm
            .id(proto::identificator::RequestId {})?
            .id;
        if !newid.is_empty() {
            *id = Some(newid);
        }
        match id {
            Some(id) => comm.id(proto::usbsas::ResponseId { id: id.clone() })?,
            None => comm.id(proto::usbsas::ResponseId { id: "".into() })?,
        }
        Ok(())
    }

    fn forward_bitvec(&mut self) -> Result<()> {
        loop {
            let rep = self
                .files2fs
                .comm
                .bitvec(proto::writefs::RequestBitVec {})?;
            self.fs2dev
                .comm
                .loadbitvec(proto::fs2dev::RequestLoadBitVec {
                    chunk: rep.chunk,
                    last: rep.last,
                })?;
            if rep.last {
                break;
            }
        }
        Ok(())
    }

    fn end_all(&mut self) -> Result<()> {
        trace!("req end");
        if let Some(ref mut analyzer) = self.analyzer {
            if let Err(err) = analyzer.comm.end(proto::analyzer::RequestEnd {}) {
                error!("Couldn't end analyzer: {}", err);
            };
        };
        if let Err(err) = self
            .identificator
            .comm
            .end(proto::identificator::RequestEnd {})
        {
            error!("Couldn't end identificator: {}", err);
        };
        if let Err(err) = self.cmdexec.comm.end(proto::cmdexec::RequestEnd {}) {
            error!("Couldn't end cmdexec: {}", err);
        };
        if let Err(err) = self.files2fs.comm.end(proto::writefs::RequestEnd {}) {
            error!("Couldn't end files2fs: {}", err);
        };
        if self.files2tar.locked {
            self.files2tar.comm.write_all(&[1_u8]).ok();
        }
        if let Err(err) = self.files2tar.comm.end(proto::writetar::RequestEnd {}) {
            error!("Couldn't end files2tar: {}", err);
        };
        if let Err(err) = self.filter.comm.end(proto::filter::RequestEnd {}) {
            error!("Couldn't end filter: {}", err);
        };
        if self.fs2dev.locked {
            self.fs2dev.comm.write_all(&(0_u64).to_ne_bytes()).ok();
        }
        if let Err(err) = self.fs2dev.comm.end(proto::fs2dev::RequestEnd {}) {
            error!("Couldn't end fs2dev: {}", err);
        };
        if let Err(err) = self.scsi2files.comm.end(proto::files::RequestEnd {}) {
            error!("Couldn't end scsi2files: {}", err);
        };
        if self.tar2files.locked {
            self.tar2files.comm.write_all(&[0_u8]).ok();
        }
        if let Err(err) = self.tar2files.comm.end(proto::files::RequestEnd {}) {
            error!("Couldn't end tar2files: {}", err);
        };
        if let Err(err) = self.uploader.comm.end(proto::uploader::RequestEnd {}) {
            error!("Couldn't end uploader: {}", err);
        };
        if let Err(err) = self.usbdev.comm.end(proto::usbdev::RequestEnd {}) {
            error!("Couldn't end usbdev: {}", err);
        };
        Ok(())
    }

    fn wait_all(&mut self) -> Result<()> {
        debug!("waiting children");
        if let Some(ref mut analyzer) = self.analyzer {
            trace!("waiting analyzer");
            if let Err(err) = analyzer.wait() {
                error!("Waiting analyzer failed: {}", err);
            };
        };
        trace!("waiting identificator");
        if let Err(err) = self.identificator.wait() {
            error!("Waiting identificator failed: {}", err);
        };
        trace!("waiting cmdexec");
        if let Err(err) = self.cmdexec.wait() {
            error!("Waiting cmdexec failed: {}", err);
        };
        trace!("waiting files2fs");
        if let Err(err) = self.files2fs.wait() {
            error!("Waiting files2fs failed: {}", err);
        };
        trace!("waiting files2tar");
        if let Err(err) = self.files2tar.wait() {
            error!("Waiting files2tar failed: {}", err);
        };
        trace!("waiting filter");
        if let Err(err) = self.filter.wait() {
            error!("Waiting filter failed: {}", err);
        };
        trace!("waiting fs2dev");
        if let Err(err) = self.fs2dev.wait() {
            error!("Waiting fs2dev failed: {}", err);
        };
        trace!("waiting scsi2files");
        if let Err(err) = self.scsi2files.wait() {
            error!("Waiting scsi2files failed: {}", err);
        };
        trace!("waiting tar2files");
        if let Err(err) = self.tar2files.wait() {
            error!("Waiting tar2files failed: {}", err);
        };
        trace!("waiting uploader");
        if let Err(err) = self.uploader.wait() {
            error!("Waiting uploader failed: {}", err);
        };
        trace!("waiting usbdev");
        if let Err(err) = self.usbdev.wait() {
            error!("Waiting usbdev failed: {}", err);
        };
        Ok(())
    }

    fn end_wait_all(&mut self, comm: &mut Comm<proto::usbsas::Request>) -> Result<()> {
        trace!("req end");
        self.end_all()?;
        self.wait_all()?;
        comm.end(proto::usbsas::ResponseEnd {})?;
        Ok(())
    }
}

pub struct Usbsas {
    comm: Comm<proto::usbsas::Request>,
    children: Children,
    state: State,
}

impl Usbsas {
    fn new(
        comm: Comm<proto::usbsas::Request>,
        config_path: &str,
        out_tar: &str,
        out_fs: &str,
        analyze: bool,
    ) -> Result<Self> {
        trace!("init");
        let mut pipes_read = vec![];
        let mut pipes_write = vec![];

        pipes_read.push(comm.input_fd());
        pipes_write.push(comm.output_fd());

        let identificator = UsbsasChildSpawner::new()
            .spawn::<usbsas_identificator::Identificator, proto::identificator::Request>()?;
        pipes_read.push(identificator.comm.input_fd());
        pipes_write.push(identificator.comm.output_fd());

        let cmdexec = UsbsasChildSpawner::new()
            .arg(out_tar)
            .arg(out_fs)
            .arg(config_path)
            .spawn::<usbsas_cmdexec::CmdExec, proto::cmdexec::Request>()?;
        pipes_read.push(cmdexec.comm.input_fd());
        pipes_write.push(cmdexec.comm.output_fd());

        let usbdev = UsbsasChildSpawner::new()
            .arg(config_path)
            .spawn::<UsbDev, proto::usbdev::Request>()?;
        pipes_read.push(usbdev.comm.input_fd());
        pipes_write.push(usbdev.comm.output_fd());

        let scsi2files = UsbsasChildSpawner::new()
            .spawn::<usbsas_scsi2files::Scsi2Files, proto::files::Request>()?;
        pipes_read.push(scsi2files.comm.input_fd());
        pipes_write.push(scsi2files.comm.output_fd());

        let files2tar = UsbsasChildSpawner::new()
            .arg(out_tar)
            .wait_on_startup()
            .spawn::<usbsas_files2tar::Files2Tar, proto::writetar::Request>()?;
        pipes_read.push(files2tar.comm.input_fd());
        pipes_write.push(files2tar.comm.output_fd());

        let files2fs = UsbsasChildSpawner::new()
            .arg(out_fs)
            .spawn::<usbsas_files2fs::Files2Fs, proto::writefs::Request>()?;
        pipes_read.push(files2fs.comm.input_fd());
        pipes_write.push(files2fs.comm.output_fd());

        let filter = UsbsasChildSpawner::new()
            .arg(config_path)
            .spawn::<usbsas_filter::Filter, proto::filter::Request>()?;
        pipes_read.push(filter.comm.input_fd());
        pipes_write.push(filter.comm.output_fd());

        let fs2dev = UsbsasChildSpawner::new()
            .arg(out_fs)
            .wait_on_startup()
            .spawn::<usbsas_fs2dev::Fs2Dev, proto::fs2dev::Request>()?;
        pipes_read.push(fs2dev.comm.input_fd());
        pipes_write.push(fs2dev.comm.output_fd());

        let tar2files = UsbsasChildSpawner::new()
            .arg(out_tar)
            .wait_on_startup()
            .spawn::<usbsas_tar2files::Tar2Files, proto::files::Request>()?;
        pipes_read.push(tar2files.comm.input_fd());
        pipes_write.push(tar2files.comm.output_fd());

        let uploader = UsbsasChildSpawner::new()
            .arg(out_tar)
            .arg(config_path)
            .spawn::<usbsas_net::Uploader, proto::uploader::Request>()?;
        pipes_read.push(uploader.comm.input_fd());
        pipes_write.push(uploader.comm.output_fd());

        let analyzer = if analyze {
            let analyzer = UsbsasChildSpawner::new()
                .arg(out_tar)
                .arg(config_path)
                .spawn::<usbsas_net::Analyzer, proto::analyzer::Request>()?;
            pipes_read.push(analyzer.comm.input_fd());
            pipes_write.push(analyzer.comm.output_fd());

            Some(analyzer)
        } else {
            None
        };

        trace!("enter seccomp");
        usbsas_privileges::usbsas::drop_priv(pipes_read, pipes_write)?;

        let children = Children {
            analyzer,
            identificator,
            cmdexec,
            files2fs,
            files2tar,
            filter,
            fs2dev,
            scsi2files,
            tar2files,
            uploader,
            usbdev,
        };

        Ok(Usbsas {
            comm,
            children,
            state: State::Init(InitState {}),
        })
    }

    fn main_loop(self) -> Result<()> {
        let (mut comm, mut children, mut state) = (self.comm, self.children, self.state);
        loop {
            state = match state.run(&mut comm, &mut children) {
                Ok(State::End) => break,
                Ok(state) => state,
                Err(err) => {
                    error!("state run error: {}, waiting end", err);
                    comm.error(proto::usbsas::ResponseError {
                        err: format!("run error: {}", err),
                    })?;
                    State::WaitEnd(WaitEndState {})
                }
            }
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let cmd = clap::Command::new("usbsas-usbsas")
        .arg(
            clap::Arg::new("config")
                .short('c')
                .long("config")
                .help("Path of the configuration file")
                .num_args(1)
                .default_value(usbsas_utils::USBSAS_CONFIG)
                .required(false),
        )
        .arg(
            clap::Arg::new("outtar")
                .value_name("OUT_TAR")
                .index(1)
                .help("Output tar filename")
                .num_args(1)
                .required(true),
        )
        .arg(
            clap::Arg::new("outfs")
                .value_name("OUT_FS")
                .index(2)
                .help("Output fs filename")
                .num_args(1)
                .required(true),
        )
        .arg(
            clap::Arg::new("analyze")
                .short('a')
                .long("analyze")
                .help("Analyze files with antivirus server")
                .num_args(0),
        );

    #[cfg(feature = "log-json")]
    let cmd = cmd.arg(
        clap::Arg::new("sessionid")
            .short('s')
            .long("sessionid")
            .help("Session id")
            .num_args(1)
            .required(true),
    );

    let matches = cmd.get_matches();
    let config = matches.get_one::<String>("config").unwrap();
    let outtar = matches.get_one::<String>("outtar").unwrap();
    let outfs = matches.get_one::<String>("outfs").unwrap();

    #[cfg(feature = "log-json")]
    usbsas_utils::log::init_logger(Arc::new(RwLock::new(
        matches.get_one::<String>("sessionid").unwrap().to_string(),
    )));

    #[cfg(not(feature = "log-json"))]
    usbsas_utils::log::init_logger();

    info!("Starting usbsas");

    let comm = Comm::from_env()?;

    let usbsas = Usbsas::new(comm, config, outtar, outfs, matches.contains_id("analyze"))?;

    usbsas.main_loop()?;

    trace!("stop");
    Ok(())
}
