use crate::coordinate_system::geographic::LLBBox;
use crate::osm_parser::OsmData;
use serde_json::json;
use std::collections::HashMap;
use rusqlite::{Connection, Result as SqliteResult};

/// GeoPackage 几何类型常量
const GPB_MAGIC: &[u8] = b"GPB";

/// 从 GeoPackage 文件中读取 OSM 数据，可选按 bbox 过滤
pub fn read_gpkg_file(
    file_path: &str,
    filter_bbox: Option<&LLBBox>,
) -> Result<OsmData, Box<dyn std::error::Error>> {
    println!("Reading GeoPackage file: {}", file_path);
    if let Some(bbox) = filter_bbox {
        println!("  Filter bbox: {:?}", bbox);
    }

    // 打开 GeoPackage 文件（SQLite 数据库）
    let conn = Connection::open(file_path)?;

    // 获取所有几何表
    let tables = get_geometry_tables(&conn)?;
    println!("Found {} vector tables", tables.len());

    let mut elements = Vec::new();

    // 处理每个表
    for table in tables {
        println!("Processing table: {}", table);
        process_table(&conn, &table, &mut elements, filter_bbox)?;
    }

    // 构建 OSM 数据结构
    let osm_data = OsmData {
        elements: serde_json::from_value(json!(elements))?,
        remark: None
    };

    Ok(osm_data)
}

/// 获取 GeoPackage 中的所有几何表
fn get_geometry_tables(conn: &Connection) -> SqliteResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT table_name FROM gpkg_geometry_columns")?;
    let table_iter = stmt.query_map([], |row| row.get(0))?;
    
    let mut tables = Vec::new();
    for table in table_iter {
        tables.push(table?);
    }
    
    Ok(tables)
}

/// 处理单个几何表
fn process_table(
    conn: &Connection,
    table_name: &str,
    elements: &mut Vec<serde_json::Value>,
    filter_bbox: Option<&LLBBox>,
) -> Result<(), Box<dyn std::error::Error>> {
    // 获取几何列名
    let geometry_column = get_geometry_column(conn, table_name)?;

    // 获取表的列信息（排除几何列）
    let _columns = get_data_columns(conn, table_name, &geometry_column)?;

    // 构建查询语句
    let query = format!("SELECT * FROM {}", table_name);
    let mut stmt = conn.prepare(&query)?;

    let column_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let _column_count = stmt.column_count();
    let mut rows = stmt.query([])?;
    let mut feature_id = 1;

    while let Some(row) = rows.next()? {
        // 查找几何列索引并提取数据
        let mut geom_data: Option<Vec<u8>> = None;
        let mut properties = HashMap::new();

        for (idx, col_name) in column_names.iter().enumerate() {
            if *col_name == geometry_column {
                geom_data = row.get(idx)?;
            } else {
                // 提取属性值
                if let Ok(val) = row.get::<_, Option<String>>(idx) {
                    if let Some(val) = val {
                        properties.insert(col_name.to_string(), val);
                    }
                }
            }
        }

        // 解析几何数据
        if let Some(geom_blob) = geom_data {
            if let Ok(geometry) = parse_gpb_geometry(&geom_blob) {
                // bbox 过滤
                if let Some(bbox) = filter_bbox {
                    if !geometry.intersects_bbox(bbox) {
                        continue;
                    }
                }

                match geometry {
                    Geometry::Point(point) => {
                        elements.push(json!({
                            "type": "node",
                            "id": feature_id,
                            "lat": point.lat,
                            "lon": point.lon,
                            "tags": properties
                        }));
                        feature_id += 1;
                    },
                    Geometry::LineString(mut points) => {
                        let node_ids: Vec<u64> = points.drain(..).map(|p| {
                            let nid = feature_id;
                            elements.push(json!({
                                "type": "node",
                                "id": nid,
                                "lat": p.lat,
                                "lon": p.lon,
                                "tags": {}
                            }));
                            feature_id += 1;
                            nid
                        }).collect();

                        elements.push(json!({
                            "type": "way",
                            "id": feature_id,
                            "nodes": node_ids,
                            "tags": properties
                        }));
                        feature_id += 1;
                    },
                    Geometry::Polygon(mut rings) => {
                        if let Some(mut exterior) = rings.first_mut().cloned() {
                            let node_ids: Vec<u64> = exterior.drain(..).map(|p| {
                                let nid = feature_id;
                                elements.push(json!({
                                    "type": "node",
                                    "id": nid,
                                    "lat": p.lat,
                                    "lon": p.lon,
                                    "tags": {}
                                }));
                                feature_id += 1;
                                nid
                            }).collect();

                            elements.push(json!({
                                "type": "way",
                                "id": feature_id,
                                "nodes": node_ids,
                                "tags": properties
                            }));
                            feature_id += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// 获取几何列名
fn get_geometry_column(conn: &Connection, table_name: &str) -> SqliteResult<String> {
    let mut stmt = conn.prepare(
        "SELECT column_name FROM gpkg_geometry_columns WHERE table_name = ?"
    )?;
    stmt.query_row([table_name], |row| row.get(0))
}

/// 获取数据列（非几何列）
fn get_data_columns(
    conn: &Connection,
    table_name: &str,
    geometry_column: &str
) -> SqliteResult<Vec<String>> {
    let query = format!("PRAGMA table_info({})", table_name);
    let mut stmt = conn.prepare(&query)?;
    let column_iter = stmt.query_map([], |row| {
        let name: String = row.get(1)?;
        Ok(name)
    })?;
    
    let mut columns = Vec::new();
    for column in column_iter {
        let col = column?;
        if col != geometry_column && col != "id" {
            columns.push(col);
        }
    }
    
    Ok(columns)
}

/// 表示一个地理坐标点
#[derive(Clone)]
struct Point {
    lat: f64,
    lon: f64,
}

/// 几何类型枚举
enum Geometry {
    Point(Point),
    LineString(Vec<Point>),
    Polygon(Vec<Vec<Point>>),
}

impl Geometry {
    /// 计算几何外包框并与给定 bbox 做相交检测
    fn intersects_bbox(&self, bbox: &LLBBox) -> bool {
        let (min_lat, min_lon, max_lat, max_lon) = match self {
            Geometry::Point(p) => (p.lat, p.lon, p.lat, p.lon),
            Geometry::LineString(points) => {
                let mut min_lat = f64::INFINITY;
                let mut min_lon = f64::INFINITY;
                let mut max_lat = f64::NEG_INFINITY;
                let mut max_lon = f64::NEG_INFINITY;
                for p in points {
                    min_lat = min_lat.min(p.lat);
                    min_lon = min_lon.min(p.lon);
                    max_lat = max_lat.max(p.lat);
                    max_lon = max_lon.max(p.lon);
                }
                (min_lat, min_lon, max_lat, max_lon)
            }
            Geometry::Polygon(rings) => {
                let mut min_lat = f64::INFINITY;
                let mut min_lon = f64::INFINITY;
                let mut max_lat = f64::NEG_INFINITY;
                let mut max_lon = f64::NEG_INFINITY;
                for ring in rings {
                    for p in ring {
                        min_lat = min_lat.min(p.lat);
                        min_lon = min_lon.min(p.lon);
                        max_lat = max_lat.max(p.lat);
                        max_lon = max_lon.max(p.lon);
                    }
                }
                (min_lat, min_lon, max_lat, max_lon)
            }
        };

        max_lat >= bbox.min().lat()
            && min_lat <= bbox.max().lat()
            && max_lon >= bbox.min().lng()
            && min_lon <= bbox.max().lng()
    }
}

/// 解析 GeoPackage 二进制几何数据 (GPB)
fn parse_gpb_geometry(blob: &[u8]) -> Result<Geometry, Box<dyn std::error::Error>> {
    if blob.len() < 8 {
        return Err("Geometry blob too short".into());
    }
    
    // 检查 GPB 魔术头
    if &blob[0..3] != GPB_MAGIC {
        // 可能已经是 WKB 格式，尝试直接解析
        return parse_wkb(blob);
    }
    
    // 解析 GPB 头部
    let _version = blob[3];
    let flags = blob[4];
    
    // 检查标志位中的 envelope 存在性
    let has_envelope = (flags & 0b0000_0010) != 0;
    let envelope_size = match flags & 0b0000_0100 {
        0 => 0,
        4 => 32,  // XY
        8 => 48,  // XYZ 或 XYM
        12 => 64, // XYZM
        _ => 32,
    };
    
    // SRS ID (4 bytes, 小端序)
    let _srs_id = i32::from_le_bytes([blob[5], blob[6], blob[7], blob[8]]);
    
    // 计算 WKB 起始偏移
    let wkb_offset = 5 + 4 + if has_envelope { envelope_size } else { 0 };
    
    if blob.len() <= wkb_offset {
        return Err("Invalid GPB geometry".into());
    }
    
    // 解析 WKB 部分
    parse_wkb(&blob[wkb_offset..])
}

/// 解析 WKB (Well-Known Binary) 几何数据
fn parse_wkb(data: &[u8]) -> Result<Geometry, Box<dyn std::error::Error>> {
    if data.len() < 5 {
        return Err("WKB data too short".into());
    }
    
    // 字节序
    let little_endian = data[0] == 1;
    
    // 几何类型
    let geom_type = if little_endian {
        u32::from_le_bytes([data[1], data[2], data[3], data[4]])
    } else {
        u32::from_be_bytes([data[1], data[2], data[3], data[4]])
    };
    
    // 解析坐标（假设是 WGS84）
    match geom_type {
        1 => parse_wkb_point(&data[5..], little_endian),
        2 => parse_wkb_linestring(&data[5..], little_endian),
        3 => parse_wkb_polygon(&data[5..], little_endian),
        _ => Err(format!("Unsupported WKB geometry type: {}", geom_type).into()),
    }
}

/// 解析 WKB Point
fn parse_wkb_point(data: &[u8], little_endian: bool) -> Result<Geometry, Box<dyn std::error::Error>> {
    if data.len() < 16 {
        return Err("WKB Point data too short".into());
    }
    
    let lon = read_f64(&data[0..8], little_endian);
    let lat = read_f64(&data[8..16], little_endian);
    
    Ok(Geometry::Point(Point { lat, lon }))
}

/// 解析 WKB LineString
fn parse_wkb_linestring(data: &[u8], little_endian: bool) -> Result<Geometry, Box<dyn std::error::Error>> {
    if data.len() < 4 {
        return Err("WKB LineString data too short".into());
    }
    
    let num_points = if little_endian {
        u32::from_le_bytes([data[0], data[1], data[2], data[3]])
    } else {
        u32::from_be_bytes([data[0], data[1], data[2], data[3]])
    } as usize;
    
    let mut points = Vec::with_capacity(num_points);
    let mut offset = 4;
    
    for _ in 0..num_points {
        if data.len() < offset + 16 {
            return Err("WKB LineString point data truncated".into());
        }
        
        let lon = read_f64(&data[offset..offset+8], little_endian);
        let lat = read_f64(&data[offset+8..offset+16], little_endian);
        points.push(Point { lat, lon });
        offset += 16;
    }
    
    Ok(Geometry::LineString(points))
}

/// 解析 WKB Polygon
fn parse_wkb_polygon(data: &[u8], little_endian: bool) -> Result<Geometry, Box<dyn std::error::Error>> {
    if data.len() < 4 {
        return Err("WKB Polygon data too short".into());
    }
    
    let num_rings = if little_endian {
        u32::from_le_bytes([data[0], data[1], data[2], data[3]])
    } else {
        u32::from_be_bytes([data[0], data[1], data[2], data[3]])
    } as usize;
    
    let mut rings = Vec::with_capacity(num_rings);
    let mut offset = 4;
    
    for _ in 0..num_rings {
        if data.len() < offset + 4 {
            return Err("WKB Polygon ring count truncated".into());
        }
        
        let num_points = if little_endian {
            u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]])
        } else {
            u32::from_be_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]])
        } as usize;
        offset += 4;
        
        let mut points = Vec::with_capacity(num_points);
        for _ in 0..num_points {
            if data.len() < offset + 16 {
                return Err("WKB Polygon point data truncated".into());
            }
            
            let lon = read_f64(&data[offset..offset+8], little_endian);
            let lat = read_f64(&data[offset+8..offset+16], little_endian);
            points.push(Point { lat, lon });
            offset += 16;
        }
        
        rings.push(points);
    }
    
    Ok(Geometry::Polygon(rings))
}

/// 读取 f64 值
fn read_f64(bytes: &[u8], little_endian: bool) -> f64 {
    if little_endian {
        f64::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
    } else {
        f64::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]])
    }
}
