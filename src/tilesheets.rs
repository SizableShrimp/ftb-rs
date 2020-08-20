use std::{
    borrow::ToOwned,
    cmp::max,
    collections::{HashMap, HashSet},
    fs::File,
    io::{stdin, BufRead, BufReader, BufWriter, Read, Write},
    mem::swap,
    path::{Path, PathBuf},
    process::{exit, Command},
};

use image::{self, ImageBuffer, RgbaImage};
use mediawiki::{tilesheet::Tilesheet, Csrf, Mediawiki, Token};
use regex::Regex;
use walkdir::WalkDir;

use crate::{decode_srgb, encode_srgb, fix_translucent, resize, save, FloatImage};

struct Sheet {
    size: u32,
    img: RgbaImage,
}

impl Sheet {
    fn new(size: u32) -> Sheet {
        let img = ImageBuffer::new(size, size);
        Sheet { size, img }
    }
    fn load(data: &[u8], size: u32) -> Sheet {
        let img = image::load_from_memory(data).unwrap();
        Sheet {
            size,
            img: img.to_rgba(),
        }
    }
    fn grow(&mut self, w: u32, h: u32) {
        let mut img = ImageBuffer::new(w, h);
        for (x, y, &pix) in self.img.enumerate_pixels() {
            img.put_pixel(x, y, pix);
        }
        swap(&mut self.img, &mut img);
    }
    fn insert(&mut self, x: u32, y: u32, img: &FloatImage) {
        let (width, height) = img.dimensions();
        assert_eq!(width, height);
        let img = resize(img, self.size, self.size);
        let img = encode_srgb(&img);
        let (w, h) = self.img.dimensions();
        if (x + 1) * self.size > w || (y + 1) * self.size > h {
            let (nw, nh) = (max((x + 1) * self.size, w), max((y + 1) * self.size, h));
            self.grow(nw, nh)
        }
        let (x, y) = (x * self.size, y * self.size);
        for (xx, yy, &pix) in img.enumerate_pixels() {
            self.img.put_pixel(x + xx, y + yy, pix);
        }
    }
}

#[derive(Debug)]
struct Tile {
    x: u32,
    y: u32,
    id: Option<u64>,
}

struct TilesheetManager {
    mw: Mediawiki,
    name: String,
    tiles: HashMap<String, Tile>,
    entries: HashMap<(u32, u32), String>,
    renames: HashMap<String, String>,
    added: Vec<String>,
    missing: HashSet<String>,
    deleted: Vec<u64>,
    tilesheets: Vec<Sheet>,
    paths: Vec<PathBuf>,
    next: (u32, u32),
}

impl TilesheetManager {
    fn new(name: &str) -> TilesheetManager {
        println!("Starting up tilesheet manager.");
        TilesheetManager {
            mw: Mediawiki::login_path("ftb.json").unwrap(),
            name: name.to_owned(),
            tiles: HashMap::new(),
            entries: HashMap::new(),
            renames: load_renames(name),
            added: Vec::new(),
            missing: HashSet::new(),
            deleted: Vec::new(),
            tilesheets: Vec::new(),
            paths: Vec::new(),
            next: (0, 0),
        }
    }
    fn import_tilesheets(&mut self) {
        println!("Checking for existing tilesheet.");
        let sheet = self.mw.query_sheets().into_iter().find(|x| {
            x.as_ref()
                .ok()
                .and_then(|x| x.get("mod"))
                .and_then(|x| x.as_str())
                .map_or(false, |x| x == self.name)
        });
        if let Some(Ok(sheet)) = sheet {
            let sizes: Vec<u64> = sheet["sizes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_u64().unwrap())
                .collect();
            println!("Existing tilesheet sizes: {:?}", sizes);
            println!("Importing existing tilesheet images.");
            for size in sizes {
                if let Some(data) = self
                    .mw
                    .download_file(&format!("Tilesheet {} {}.png", self.name, size))
                    .unwrap()
                {
                    self.tilesheets.push(Sheet::load(&data, size as u32))
                } else {
                    println!("WARNING: No tilesheet image found for size {}!", size);
                    self.tilesheets.push(Sheet::new(size as u32));
                }
            }
        } else {
            println!("No tilesheet found. Please specify desired sizes separated by commas:");
            let mut sizes = String::new();
            stdin().read_line(&mut sizes).unwrap();
            let sizes = sizes.split(',').map(str::trim).collect::<Vec<_>>();
            for size in &sizes {
                self.tilesheets.push(Sheet::new(size.parse().unwrap()));
            }
            let token = self.mw.get_token().unwrap();
            self.mw
                .create_sheet(&token, &self.name, &sizes.join("|"))
                .unwrap();
        }
    }
    fn import_tiles(&mut self) {
        println!("Importing tiles.");
        for tile in self.mw.query_tiles(Some(&*self.name)) {
            let tile = match tile {
                Ok(tile) => tile,
                Err(e) => {
                    println!("WARNING: Error while querying tiles {:?}", e);
                    continue;
                }
            };
            let x = tile["x"].as_u64().unwrap() as u32;
            let y = tile["y"].as_u64().unwrap() as u32;
            let id = tile["id"].as_u64().unwrap();
            let name = tile["name"].as_str().unwrap();
            self.tiles
                .insert(name.to_owned(), Tile { x, y, id: Some(id) });
            self.entries.insert((x, y), name.to_owned());
            self.missing.insert(name.to_owned());
        }
    }
    fn check_changes(&mut self) {
        println!("Checking tiles.");
        let path = Path::new(r"work/tilesheets").join(&self.name);
        for entry in WalkDir::new(&path) {
            let entry = entry.unwrap();
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|x| x.to_str()) != Some("png") {
                continue;
            }
            let name = path.file_stem().unwrap().to_str().unwrap();
            let name = match self.renames.get(name) {
                Some(name) => {
                    if name.is_empty() {
                        continue;
                    }
                    name.clone()
                }
                None => name.to_owned(),
            };
            if name.contains(&['_', '[', ']'][..]) {
                println!("ERROR: Illegal name: {:?}", name);
                exit(1);
            }
            self.missing.remove(&name);
            if !self.tiles.contains_key(&name) {
                self.added.push(name);
            }
        }
    }
    fn confirm_changes(&mut self) {
        let mut additions = BufWriter::new(File::create("work/tilesheets/additions.txt").unwrap());
        let mut missing = BufWriter::new(File::create(r"work/tilesheets/missing.txt").unwrap());
        let _ = File::create(r"work/tilesheets/todelete.txt").unwrap();
        for tile in &self.added {
            writeln!(&mut additions, "{}", tile).unwrap();
        }
        for tile in &self.missing {
            writeln!(&mut missing, "{}", tile).unwrap();
        }
        drop(additions);
        drop(missing);
        println!("Please confirm that the tiles being added in additions.txt are correct.");
        println!("Also please check over the tiles in missing.txt and ensure that not updating them was intentional.");
        println!("If there are tiles in missing.txt that you no longer wish to keep, please copy them to todelete.txt.");
        println!("If you need to make any changes to the tiles or renames.txt please restart this program.");
        println!("When you are done, please enter \"continue\".");
        let mut response = String::new();
        stdin().read_line(&mut response).unwrap();
        if response.trim().to_lowercase() != "continue" {
            println!("Aborting!");
            exit(1);
        }
    }
    fn record_deletions(&mut self) {
        let todelete = BufReader::new(File::open(r"work/tilesheets/todelete.txt").unwrap());
        for line in todelete.lines() {
            let name = line.unwrap();
            if let Some(tile) = self.tiles.remove(&name) {
                self.deleted.push(tile.id.unwrap());
                self.entries.remove(&(tile.x, tile.y));
            } else {
                println!(
                    "ERROR: Requested to delete tile that doesn't exist {:?}",
                    name
                );
            }
        }
    }
    fn lookup(&mut self, name: &str) -> (u32, u32) {
        if let Some(tile) = self.tiles.get(name) {
            return (tile.x, tile.y);
        }
        let pos = loop {
            let pos = if self.next.1 < self.next.0 {
                (self.next.1, self.next.0)
            } else {
                (self.next.0, self.next.1 - self.next.0)
            };
            if self.entries.get(&pos).is_none() {
                break pos;
            }
            self.next.1 += 1;
            if self.next.1 > self.next.0 * 2 {
                self.next.0 += 1;
                self.next.1 = 0;
            }
        };
        self.tiles.insert(
            name.to_owned(),
            Tile {
                x: pos.0,
                y: pos.1,
                id: None,
            },
        );
        self.entries.insert(pos, name.to_owned());
        (pos.0, pos.1)
    }
    fn update(&mut self) {
        println!("Updating tilesheet with new tiles.");
        let path = Path::new(r"work/tilesheets").join(&self.name);
        for entry in WalkDir::new(&path) {
            let entry = entry.unwrap();
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|x| x.to_str()) != Some("png") {
                continue;
            }
            let name = path.file_stem().unwrap().to_str().unwrap();
            let name = match self.renames.get(name) {
                Some(name) => {
                    if name.is_empty() {
                        continue;
                    }
                    name.clone()
                }
                None => name.to_owned(),
            };
            if name.contains(&['_', '[', ']'][..]) {
                println!("ERROR: Illegal name: {:?}", name);
                exit(1);
            }
            let mut img = image::open(&path).unwrap().to_rgba();
            fix_translucent(&mut img);
            let img = decode_srgb(&img);
            let (x, y) = self.lookup(&name);
            for tilesheet in &mut self.tilesheets {
                tilesheet.insert(x, y, &img);
            }
        }
    }
    fn optimize(&mut self) {
        println!("Optimizing tilesheets");
        let mut temp = Vec::new();
        let optipng = self
            .tilesheets
            .iter()
            .map(|tilesheet| {
                let name = format!("Tilesheet {} {}.png", self.name, tilesheet.size);
                let path = Path::new(r"work/tilesheets").join(name);
                save(&tilesheet.img, &path);
                temp.push(path.to_owned());
                Command::new("optipng").arg(path).spawn().unwrap()
            })
            .collect::<Vec<_>>();
        self.paths.extend(temp);
        for mut child in optipng {
            child.wait().unwrap();
        }
    }
    fn upload_sheets(&self) {
        let token = self.mw.get_token().unwrap();
        for path in &self.paths {
            self.upload_path(path, None, &token, false);
        }
    }
    fn upload_path(
        &self,
        path: &PathBuf,
        filekey: Option<String>,
        token: &Token<Csrf>,
        ignore_warnings: bool,
    ) {
        let filename = path.file_name().unwrap().to_str().unwrap();
        if !ignore_warnings {
            println!("Uploading \"{}\"", filename);
        }
        let result;
        let text = "[[Category:Tilesheets]]";
        if let Some(key) = filekey {
            result =
                self.mw
                    .upload_filekey(filename, key.as_str(), &token, Some(text), ignore_warnings);
        } else {
            result = self
                .mw
                .upload_file(filename, &path, &token, Some(text), ignore_warnings);
        }

        if let Ok(v) = result {
            if v.get("errors").is_none() {
                let value = v.get("upload").unwrap();
                let response = value.get("result").unwrap().as_str().unwrap();
                let filekey = &value["filekey"];
                if response == "Warning" {
                    let warnings = value.get("warnings").unwrap();
                    let map = warnings.as_object().unwrap();
                    let reupload = map
                        .get("exists")
                        .and_then(|v| v.as_str())
                        .map(|s| s == filename)
                        .unwrap_or(false);
                    if map.len() == 1 && reupload {
                        // Warning is about the page already existing, but we are updating it.
                        self.upload_path(
                            path,
                            filekey.as_str().map(|s| s.to_string()),
                            token,
                            true,
                        );
                        return;
                    }
                    println!("The API returned warnings when attempting to upload the file.");
                    println!("Warnings: {:?}", serde_json::to_string(warnings).unwrap());
                    println!(
                        "Would you like to try to upload the file again and ignore these warnings? y/n"
                    );
                    let mut input = String::new();
                    stdin().read_line(&mut input).unwrap();
                    input = input.trim().to_owned();
                    if input.to_ascii_lowercase() == "y" {
                        self.upload_path(
                            path,
                            filekey.as_str().map(|s| s.to_string()),
                            token,
                            true,
                        );
                    } else {
                        println!("Please manually upload {}.", filename);
                    }
                } else if response == "Success" {
                    println!("Successfully uploaded {}", filename);
                }
            } else {
                println!(
                    "An error occurred when uploading \"{}\". Please manually upload the file.",
                    filename
                );
                let errors = v.get("errors").unwrap().as_array();
                if let Some(vector) = errors {
                    let mut count = 1;
                    for error in vector {
                        let code = error["code"].as_str().unwrap_or("");
                        let description = error["*"].as_str().unwrap_or("");
                        println!(
                            "Error response from API ({}): {} - {}",
                            count, code, description
                        );
                        count += 1;
                    }
                } else {
                    println!("The API didn't return any error objects to display.");
                }
            }
        } else {
            println!(
                "An error occurred when uploading \"{}\". Please manually upload the file.",
                filename
            );
            println!("Error locally: {:?}", result);
        }
    }
    fn delete_tiles(&self) {
        println!("Deleting old tiles that are no longer needed.");
        let token = self.mw.get_token().unwrap();
        for chunk in self.deleted.chunks(50) {
            let tiles = chunk
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join("|");
            if let Err(e) = self.mw.delete_tiles(&token, &tiles) {
                println!("ERROR: {:?}", e);
            }
        }
    }
    fn add_tiles(&self) {
        println!("Adding new tiles.");
        let token = self.mw.get_token().unwrap();
        for chunk in self.added.chunks(50) {
            let tiles = chunk
                .iter()
                .map(|name| {
                    let tile = &self.tiles[name];
                    format!("{} {} {}", tile.x, tile.y, name)
                })
                .collect::<Vec<_>>()
                .join("|");
            if let Err(e) = self.mw.add_tiles(&token, &self.name, &tiles) {
                println!("ERROR: {:?}", e);
            }
        }
    }
}

fn load_renames(name: &str) -> HashMap<String, String> {
    let path = Path::new(r"work/tilesheets").join(name);
    match File::open(&path.join("renames.txt")) {
        Ok(mut file) => {
            let reg = Regex::new("(.*)=(.*)").unwrap();
            let mut s = String::new();
            file.read_to_string(&mut s).unwrap();
            s.lines()
                .filter_map(|line| match reg.captures(line) {
                    Some(cap) => Some((cap[1].to_owned(), cap[2].to_owned())),
                    None => {
                        println!("WARNING: Invalid line in renames.txt {:?}", line);
                        None
                    }
                })
                .collect()
        }
        Err(e) => {
            println!("WARNING: Failed to load renames.txt {:?}", e);
            HashMap::new()
        }
    }
}

pub fn update_tilesheet(name: &str) {
    let mut manager = TilesheetManager::new(name);
    manager.import_tilesheets();
    manager.import_tiles();
    manager.check_changes();
    manager.confirm_changes();
    manager.record_deletions();
    manager.update();
    manager.optimize();
    manager.upload_sheets();
    manager.delete_tiles();
    manager.add_tiles();
    println!("Done");
}
