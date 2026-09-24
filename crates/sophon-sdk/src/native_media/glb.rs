//! Bounded, self-contained GLB preview validation. Never resolves external resources.
use super::{Error, decode, fail};
use gltf::{
    accessor::{DataType, Dimensions},
    mesh::Mode,
};
use serde_json::{Value, json};
use std::path::Path;

pub(super) async fn validate(executable: &Path, bytes: &[u8]) -> Result<Value, Error> {
    let invalid = || fail("Model content is not a supported self-contained GLB preview");
    let word = |offset: usize| -> Result<usize, Error> {
        Ok(u32::from_le_bytes(
            bytes
                .get(offset..offset + 4)
                .ok_or_else(invalid)?
                .try_into()
                .unwrap(),
        ) as usize)
    };
    if bytes.len() > 32 * 1024 * 1024
        || bytes.get(..4) != Some(b"glTF")
        || word(4)? != 2
        || word(8)? != bytes.len()
    {
        return Err(invalid());
    }
    let json_len = word(12)?;
    let json_end = 20usize.checked_add(json_len).ok_or_else(invalid)?;
    if json_len % 4 != 0 || bytes.get(16..20) != Some(b"JSON") {
        return Err(invalid());
    }
    let raw: Value = serde_json::from_slice(bytes.get(20..json_end).ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    let bin_len = word(json_end)?;
    let bin_start = json_end.checked_add(8).ok_or_else(invalid)?;
    if bin_len % 4 != 0
        || bytes.get(json_end + 4..bin_start) != Some(b"BIN\0")
        || bin_start.checked_add(bin_len) != Some(bytes.len())
    {
        return Err(invalid());
    }
    let bin = bytes.get(bin_start..).ok_or_else(invalid)?;
    if raw["asset"]["version"] != "2.0"
        || raw["asset"].get("minVersion").is_some_and(|v| v != "2.0")
    {
        return Err(invalid());
    }
    // Parser 1.4.1's POSITION validation follows the reference before returning
    // validation errors. Preflight it and image shapes before any typed getters.
    let accessors = raw["accessors"].as_array().ok_or_else(invalid)?;
    let meshes = raw["meshes"].as_array().ok_or_else(invalid)?;
    if accessors.len() > 16384 || meshes.is_empty() || meshes.len() > 4096 {
        return Err(invalid());
    }
    for mesh in meshes {
        for primitive in mesh["primitives"].as_array().ok_or_else(invalid)? {
            let position = primitive["attributes"]["POSITION"]
                .as_u64()
                .ok_or_else(invalid)?;
            if position >= accessors.len() as u64 || primitive.get("targets").is_some() {
                return Err(invalid());
            }
        }
    }
    if raw
        .get("skins")
        .and_then(Value::as_array)
        .is_some_and(|v| !v.is_empty())
        || raw
            .get("animations")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
    {
        return Err(invalid());
    }
    let images = raw
        .get("images")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if images.len() > 64 {
        return Err(invalid());
    }
    for image in &images {
        if image.get("uri").is_some()
            || image["bufferView"].as_u64().is_none()
            || !matches!(image["mimeType"].as_str(), Some("image/png" | "image/jpeg"))
        {
            return Err(invalid());
        }
    }
    let root = serde_json::from_value(raw).map_err(|_| invalid())?;
    let document = gltf::Document::from_json(root).map_err(|_| invalid())?;
    let buffers: Vec<_> = document.buffers().collect();
    if buffers.len() != 1
        || !matches!(buffers[0].source(), gltf::buffer::Source::Bin)
        || buffers[0].length() == 0
        || buffers[0].length() > bin.len()
        || bin.len() - buffers[0].length() > 3
        || bin[buffers[0].length()..].iter().any(|b| *b != 0)
    {
        return Err(invalid());
    }
    let bin = &bin[..buffers[0].length()];
    for view in document.views() {
        if view.length() == 0
            || view
                .offset()
                .checked_add(view.length())
                .is_none_or(|end| end > bin.len())
        {
            return Err(invalid());
        }
    }
    let mut elements = 0usize;
    for accessor in document.accessors() {
        let view = accessor.view().ok_or_else(invalid)?;
        if accessor.count() == 0
            || accessor.sparse().is_some()
            || matches!(
                accessor.dimensions(),
                Dimensions::Mat2 | Dimensions::Mat3 | Dimensions::Mat4
            )
        {
            return Err(invalid());
        }
        elements = elements.checked_add(accessor.count()).ok_or_else(invalid)?;
        let width = accessor.size();
        let stride = view.stride().unwrap_or(width);
        let component = accessor.data_type().size();
        let end = (accessor.count() - 1)
            .checked_mul(stride)
            .and_then(|n| n.checked_add(accessor.offset()))
            .and_then(|n| n.checked_add(width));
        if elements > 8_000_000
            || stride < width
            || accessor.offset() % component != 0
            || view
                .offset()
                .checked_add(accessor.offset())
                .is_none_or(|offset| offset % component != 0)
            || end.is_none_or(|end| end > view.length())
        {
            return Err(invalid());
        }
    }
    let mut primitive_count = 0usize;
    let mut triangle_count = 0usize;
    let mut decoded_vertices = 0usize;
    for mesh in document.meshes() {
        for primitive in mesh.primitives() {
            primitive_count += 1;
            let positions = primitive
                .get(&gltf::Semantic::Positions)
                .ok_or_else(invalid)?;
            if primitive_count > 16384
                || primitive.mode() != Mode::Triangles
                || positions.data_type() != DataType::F32
                || positions.dimensions() != Dimensions::Vec3
                || positions.normalized()
            {
                return Err(invalid());
            }
            decoded_vertices = decoded_vertices
                .checked_add(positions.count())
                .ok_or_else(invalid)?;
            if decoded_vertices > 8_000_000 {
                return Err(invalid());
            }
            for (_, attribute) in primitive.attributes() {
                if attribute.count() != positions.count() {
                    return Err(invalid());
                }
            }
            if let Some(indices) = primitive.indices()
                && (indices.dimensions() != Dimensions::Scalar
                    || indices.normalized()
                    || indices.view().unwrap().stride().is_some()
                    || !matches!(
                        indices.data_type(),
                        DataType::U8 | DataType::U16 | DataType::U32
                    ))
            {
                return Err(invalid());
            }
            let reader = primitive.reader(|_| Some(bin));
            let vertices: Vec<_> = reader.read_positions().ok_or_else(invalid)?.collect();
            if vertices.len() != positions.count()
                || vertices.iter().flatten().any(|n| !n.is_finite())
            {
                return Err(invalid());
            }
            let indices: Vec<u32> = if primitive.indices().is_some() {
                reader
                    .read_indices()
                    .ok_or_else(invalid)?
                    .into_u32()
                    .collect()
            } else {
                (0..vertices.len() as u32).collect()
            };
            if indices.len() < 3
                || !indices.len().is_multiple_of(3)
                || indices.iter().any(|i| *i as usize >= vertices.len())
            {
                return Err(invalid());
            }
            triangle_count = triangle_count
                .checked_add(indices.len() / 3)
                .ok_or_else(invalid)?;
            if triangle_count > 4_000_000 {
                return Err(invalid());
            }
            if !indices.chunks_exact(3).any(|tri| {
                let a = vertices[tri[0] as usize].map(f64::from);
                let b = vertices[tri[1] as usize].map(f64::from);
                let c = vertices[tri[2] as usize].map(f64::from);
                let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
                [
                    u[1] * v[2] - u[2] * v[1],
                    u[2] * v[0] - u[0] * v[2],
                    u[0] * v[1] - u[1] * v[0],
                ]
                .iter()
                .any(|n| n.abs() > 0.0)
            }) {
                return Err(invalid());
            }
        }
    }
    if primitive_count == 0 {
        return Err(invalid());
    }
    // Reject cyclic/unbounded scenes; at least one validated mesh must be reachable.
    let nodes: Vec<_> = document.nodes().collect();
    if nodes.len() > 16384 {
        return Err(invalid());
    }
    let mut active = vec![false; nodes.len()];
    let mut seen = vec![false; nodes.len()];
    let mut stack: Vec<_> = document
        .scenes()
        .flat_map(|s| s.nodes())
        .map(|n| (n.index(), false))
        .collect();
    let mut reachable = false;
    let mut visits = 0usize;
    while let Some((index, exit)) = stack.pop() {
        visits += 1;
        if visits > 65536 {
            return Err(invalid());
        }
        if exit {
            active[index] = false;
            continue;
        }
        if active[index] {
            return Err(invalid());
        }
        if seen[index] {
            continue;
        }
        seen[index] = true;
        active[index] = true;
        let node = &nodes[index];
        if node
            .transform()
            .matrix()
            .iter()
            .flatten()
            .any(|n| !n.is_finite())
        {
            return Err(invalid());
        }
        reachable |= node.mesh().is_some();
        stack.push((index, true));
        stack.extend(node.children().map(|n| (n.index(), false)));
    }
    if !reachable {
        return Err(invalid());
    }
    let mut decoded_image_bytes = 0usize;
    for image in document.images() {
        let gltf::image::Source::View { view, mime_type } = image.source() else {
            return Err(invalid());
        };
        decoded_image_bytes = decoded_image_bytes
            .checked_add(view.length())
            .ok_or_else(invalid)?;
        if decoded_image_bytes > 32 * 1024 * 1024 {
            return Err(invalid());
        }
        decode(
            executable,
            &bin[view.offset()..view.offset() + view.length()],
            if mime_type == "image/png" {
                "png"
            } else {
                "jpeg"
            },
        )
        .await?;
    }
    Ok(
        json!({"format":"glb","version":2,"meshes":document.meshes().len(),"primitives":primitive_count,"triangles":triangle_count,"vertices":decoded_vertices,"embeddedImages":images.len(),"quality":"preview"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Value, Vec<u8>) {
        let bytes = [0_f32, 0., 0., 2., 0., 0., 0., 3., 0.]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        (
            json!({"asset":{"version":"2.0"},"buffers":[{"byteLength":36}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[2,3,0]}],"meshes":[{"primitives":[{"attributes":{"POSITION":0}}]}],"nodes":[{"mesh":0}],"scenes":[{"nodes":[0]}],"scene":0}),
            bytes,
        )
    }
    fn pack(value: Value, mut bin: Vec<u8>) -> Vec<u8> {
        let mut json = serde_json::to_vec(&value).unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        while !bin.len().is_multiple_of(4) {
            bin.push(0);
        }
        let mut bytes = b"glTF".to_vec();
        bytes.extend(2_u32.to_le_bytes());
        bytes.extend(((28 + json.len() + bin.len()) as u32).to_le_bytes());
        bytes.extend((json.len() as u32).to_le_bytes());
        bytes.extend(b"JSON");
        bytes.extend(json);
        bytes.extend((bin.len() as u32).to_le_bytes());
        bytes.extend(b"BIN\0");
        bytes.extend(bin);
        bytes
    }
    #[tokio::test]
    async fn decodes_real_geometry_and_rejects_malformed_or_external_content() {
        let (root, bin) = fixture();
        let valid = pack(root.clone(), bin.clone());
        let result = validate(Path::new("ffmpeg"), &valid).await.unwrap();
        assert_eq!(result["vertices"], 3);
        assert_eq!(result["triangles"], 1);
        assert_eq!(result["quality"], "preview");
        for length in [0, 4, 12, 19, valid.len() - 1] {
            assert!(
                validate(Path::new("ffmpeg"), &valid[..length])
                    .await
                    .is_err()
            );
        }
        for case in 0..10 {
            let mut root = root.clone();
            let mut bin = bin.clone();
            match case {
                0 => root["meshes"][0]["primitives"][0]["attributes"]["POSITION"] = json!(99),
                1 => root["accessors"][0]["count"] = json!(0),
                2 => root["bufferViews"][0]["byteStride"] = json!(4),
                3 => root["accessors"][0]["byteOffset"] = json!(u64::MAX),
                4 => root["buffers"][0]["uri"] = json!("https://private.invalid/credential"),
                5 => root["nodes"][0]["children"] = json!([0]),
                6 => bin[..4].copy_from_slice(&f32::NAN.to_le_bytes()),
                7 => bin.fill(0),
                8 => root["images"] = json!([{"uri":"secret.png"}]),
                9 => root["scenes"][0]["nodes"] = json!([]),
                _ => unreachable!(),
            }
            assert!(
                validate(Path::new("ffmpeg"), &pack(root, bin))
                    .await
                    .is_err(),
                "case {case}"
            );
        }
    }
}
