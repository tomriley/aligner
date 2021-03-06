
use opencv::prelude::*;
use opencv::types::*;
use opencv::core::*;
use opencv::imgcodecs;
use opencv::imgproc::*;
use opencv::calib3d::*;
use glm::*;
use glm::ext::*;
use serde_json::json;
use std::io::prelude::*;
use std::fmt;
use log::{info, warn, debug};
use regex::Regex;
use lazy_static::*;

mod math;
mod photo;
mod images;
mod network;
mod locator;
pub mod surfaces;
mod camera_calibration;

pub struct PhysicalCamera {
    pub position: glm::Vec3,
    pub look_at: glm::Vec3, // TODO rename this to direction
    pub up_dir: glm::Vec3,
    pub calibration: camera_calibration::Calibration,
}

struct VirtualCamera {
    pub position: glm::Vec3,
    pub up_dir: glm::Vec3, // always 0, 1, 0
    pub look_at: Option<glm::Vec3>, // this is calculated during calibration
    pub fov: Option<f32> // this is calculated during calibration
}

#[derive(Clone, Copy)]
pub struct Resolution {
    width: i32,
    height: i32
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

impl Resolution {
    /// aspect ratio of projector output as a fraction (width/height)
    fn aspect_ratio(&self) -> f32 {
        self.width as f32 / self.height as f32
    }

    pub fn parse(input: &str) -> Result<Resolution, &'static str> {
        lazy_static! {
            static ref RE: Regex = Regex::new(r"(?P<width>\d+)x(?P<height>\d+)").unwrap();
        }
        let caps = RE.captures(input).expect("Failed to parse resolution input string");
        Ok(Resolution {
            width: caps["width"].parse().unwrap(),
            height: caps["height"].parse().unwrap()
        })
    }
}

/// Output camera location relative to a single 6x6 aruco marker at 0,0,0 facing into the Z axis
pub fn locate_camera(camera_cal_fname: &str, camera: Option<&str>, marker_size: f32) {
    let calibration = camera_calibration::load_calibration_file(camera_cal_fname).expect("load of calibration XML failed");
    let camera_type = match camera {
        Some(url_or_path) => {
            if url_or_path.starts_with("http") {
                photo::CameraType::RemoteHttpCamera {url: url_or_path.to_string()}
            } else {
                // TODO check early that file exists
                photo::CameraType::SingleImageFile {path: url_or_path.to_string()}
            }
        }
        None => photo::CameraType::TetheredCamera
    };
    let photo = photo::capture_photo(camera_type);
    let mut decoded = imgcodecs::imdecode(&photo, imgcodecs::IMREAD_COLOR).unwrap();
    locator::locate_aruco_marker(&calibration, &mut decoded, marker_size);
}

pub fn produce_calibration(surface: surfaces::SurfaceType, camera_cal_fname: &str, control_url: Option<&str>, camera: Option<&str>, camera_location_fname: Option<&str>, eye_position: glm::Vec3, warp_res: Resolution, projector_res: Resolution, post_to: Option<&str>) {
    let calibration = camera_calibration::load_calibration_file(camera_cal_fname).expect("load of calibration XML failed");
    let mut physical_camera = PhysicalCamera {    
        // camera position (should be suppied by user)
        position: vec3(0., 0., 0.),
        look_at: vec3(0., 1., 0.),
        up_dir: vec3(0., 0., 1.),
        calibration: calibration
    };
    if let Some(fname) = camera_location_fname {
        locator::update_physical_camera_location(&mut physical_camera, fname);
    }
    let camera_type = match camera {
        Some(url_or_path) => {
            if url_or_path.starts_with("http") {
                photo::CameraType::RemoteHttpCamera {url: url_or_path.to_string()}
            } else {
                // TODO check early that file exists
                photo::CameraType::SingleImageFile {path: url_or_path.to_string()}
            }
        }
        None => photo::CameraType::TetheredCamera
    };
    let mut virtual_camera = VirtualCamera {
        position: eye_position,
        look_at: None,
        up_dir: vec3(0.0, 1.0, 0.0),
        fov: None,
    };

    info!("projector resolution is {}", projector_res);

    let image_points = detect_image_points(&physical_camera, control_url, camera_type, warp_res);
    let scene_coords = locate_scene_coords(&surface, &physical_camera, &image_points);
    virtual_camera.look_at = Some(calculate_look_at(&surface, &image_points, &physical_camera));
    let uv_coords = generate_uv_warp_and_fov(&scene_coords, &mut virtual_camera, projector_res);
    let json = calibration_json_string(&scene_coords, &uv_coords, &virtual_camera, warp_res);
    if let Some(url) = post_to {
        network::send_command(&url, "set_calibration", &json);
    } else {
        println!("{}", json);
    }
}


fn calculate_look_at(surface: &surfaces::SurfaceType, image_points: &Vec<glm::Vec2>, physical_camera: &PhysicalCamera) -> glm::Vec3 {
    // possibly naively, we just look_at the center of the chessboard
    let mut avg = vec2(0., 0.);
    for p in image_points.iter() { avg = avg + *p }
    avg = avg / image_points.len() as f32;
    
    debug!("Projection area center point is {:?}", avg);

    surfaces::camera_to_scene(
        &surface,
        &physical_camera,
        avg,
        physical_camera.calibration.image_width,
        physical_camera.calibration.image_height
    ).unwrap()
}

fn locate_scene_coords(surface: &surfaces::SurfaceType, physical_camera: &PhysicalCamera, image_points: &Vec<glm::Vec2>) -> Vec<glm::Vec3> { 
    let mut scene_coords = vec![];

    for point in image_points.iter() {
        // Convert point in camera space to a point in 3d world space
        let scene_coord = surfaces::camera_to_scene(
            &surface,
            &physical_camera,
            *point,
            physical_camera.calibration.image_width,
            physical_camera.calibration.image_height
        ).unwrap();
        scene_coords.push(scene_coord);
    }

    scene_coords
}

fn detect_image_points(physical_camera: &PhysicalCamera, control_url: Option<&str>, camera_type: photo::CameraType, warp_res: Resolution) -> Vec<glm::Vec2> {
    // show chessboard image on first projector
    let chessboard = images::chessboard_image(warp_res.width, warp_res.height, ".png");
    match &control_url {
        Some(url) => {
            network::post_image(&url, &chessboard.to_slice(), "png").unwrap();
        },
        None => {
            info!("Please display the full-screen chessboard pattern on the projector and press any key");
            std::io::stdin().bytes().next();
            info!("Continuing...");
        }
    }

    let photo = take_undistorted_photo(&physical_camera.calibration, camera_type).expect("failed to take photo");
    locate_chessboard_corners(&photo, warp_res).expect("failed to locate chessboard corners")
}

fn generate_uv_warp_and_fov(scene_coords: &Vec<glm::Vec3>, virtual_camera: &mut VirtualCamera, projector_res: Resolution) -> Vec<glm::Vec2> {
    let trans = look_at(virtual_camera.position, virtual_camera.look_at.unwrap(), virtual_camera.up_dir);
    let mut max_rad = -1_f32;
    
    for scene_point in scene_coords.iter() {
        let eye_relative = trans * scene_point.extend(1.);
        //let rad = atan(eye_relative.y.abs() / eye_relative.z.abs());
        let rad = eye_relative.y.abs().atan2(eye_relative.z.abs());
        if rad > max_rad { max_rad = rad; }
    }
    
    virtual_camera.fov = Some(glm::degrees(max_rad) * 2.001); // FIXMEshouldn't really need to add 10% on here?
    
    info!("eyePoint = {:?} lookAt = {:?} fovY = {:?}", virtual_camera.position, virtual_camera.look_at, virtual_camera.fov.unwrap());

    let mut uv_coords = vec![];
    for &scene_coord in scene_coords.iter() {
        let target_screen_point = project_scene_point(
            scene_coord, &virtual_camera,
            projector_res.aspect_ratio()
        );

        // We now have the coord pixel of the render buffer that should be warped to the current chessboard corner
        uv_coords.push(target_screen_point);
    }
    uv_coords
}

/// Given virtual camera details, calculate normalized screen position of the point in 3D space
fn project_scene_point(scene_pos: glm::Vec3, virtual_camera: &VirtualCamera, projector_aspect_ratio: f32) -> glm::Vec2 {
    let model = glm::ext::look_at(virtual_camera.position, virtual_camera.look_at.unwrap(), virtual_camera.up_dir);
    let proj = glm::ext::perspective(
        glm::radians(virtual_camera.fov.unwrap()),
        projector_aspect_ratio,
        0.1,
        100.
    );
    
    let screen_pos = math::project(vec3(scene_pos.x, scene_pos.y, scene_pos.z), &model, &proj, vec4(0., 0., 1., 1.));
    if screen_pos.x < 0. || screen_pos.y < 0. || screen_pos.x > 1. || screen_pos.y > 1. {
        warn!("a point in the scene space projected off screen (in project_scene_point)");
    }
    
    screen_pos.truncate(2)
}


fn locate_chessboard_corners(photo: &Mat, warp_res: Resolution) -> opencv::Result<Vec<glm::Vec2>> {
    // find chessboard corners
    let mut point_buffer = VectorOfPoint2f::new();
    let board_size = Size::new(warp_res.width, warp_res.height);
    debug!("Finding chessboard corners...");
    let found = find_chessboard_corners(&photo, board_size, &mut point_buffer, CALIB_CB_ADAPTIVE_THRESH)?;
    
    // draw found chessboard corners to image file
    if false {
        let mut color = Mat::default()?;
        cvt_color(&photo, &mut color, COLOR_GRAY2BGR, 1)?;
        draw_chessboard_corners(&mut color, board_size, &point_buffer, found)?;
        imgcodecs::imwrite("alignment-corners.jpg", &color, &VectorOfi32::new())?;
    }

    if !found {
        panic!("Complete set of chessboard corners not detected");
    }

    // corner subpix analysis
    corner_sub_pix(&photo, &mut point_buffer, board_size, Size::new(-1, -1),
                     TermCriteria::new(3, 30, 0.1f64).unwrap())?; // 3 = COUNT + EPS
    
    // convert to vector of glm::Vec2
    Ok(point_buffer.iter().map(|pt| vec2(pt.x, pt.y)).collect())
}

fn take_undistorted_photo(calibration: &camera_calibration::Calibration, camera_type: photo::CameraType) -> opencv::Result<Mat> {
    // take photo
    let photo_data = photo::capture_photo(camera_type);
    let photo = imgcodecs::imdecode(&photo_data, imgcodecs::IMREAD_COLOR)?;

    // check dimentions match calibration data
    if photo.rows() != calibration.image_height || photo.cols() != calibration.image_width {
        panic!(
            "photo dimentions ({}x{}) don't match width and height in calibration file ({}x{})",
            photo.cols(), photo.rows(), calibration.image_width, calibration.image_height
        );
    }

    let mut undistorted_img = Mat::default()?;
    undistort(&photo, &mut undistorted_img, &calibration.camera_matrix, &calibration.distortion_coefficients, &calibration.camera_matrix)?;
    imgcodecs::imwrite("alignment-undistorted.jpg", &undistorted_img, &VectorOfi32::new())?;

    // convert to greyscale and invert back to expected color layout and white border
    // required for the opencv corner detection to work
    let mut gray = Mat::default()?;
    let mut inverted_img = Mat::default()?;
    cvt_color(&undistorted_img, &mut gray, COLOR_BGR2GRAY, 1)?;
    bitwise_not(&gray, &mut inverted_img, &Mat::default().unwrap())?;
    imgcodecs::imwrite("alignment-inverted.jpg", &inverted_img, &VectorOfi32::new())?;
    Ok(inverted_img)
}


fn calibration_json_string(scene_coords: &Vec<glm::Vec3>, uv_coords: &Vec<glm::Vec2>, virtual_camera: &VirtualCamera, warp_res: Resolution) -> String {
    // Build final "calibration" JSON document
    let scene: Vec<&[f32; 3]> = scene_coords.iter().map(|p| p.as_array()).collect();
    let warp: Vec<&[f32; 2]> = uv_coords.iter().map(|p| p.as_array()).collect();

    debug!("scene has {} coordinates", scene.len());
    debug!("warp has {} coordinates", warp.len());

    let json = json!({
        "fov": virtual_camera.fov,
        "eye": virtual_camera.position.as_array(),
        "lookAt": virtual_camera.look_at.unwrap().as_array(),
        "up": virtual_camera.up_dir.as_array(),
        "warpResX": warp_res.width,
        "warpResY": warp_res.height,
        "warp": warp,
        "scene": scene
    });

    serde_json::to_string_pretty(&json).unwrap()
}