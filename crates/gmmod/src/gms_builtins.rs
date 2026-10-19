//! GameMaker: Studio 1.4's built-in variables (from UndertaleModLib's BuiltinList at
//! commit 92c968f, the GMS2-only ones left out): globals are read with pushbltn, instance
//! ones with push; both are self variables with id -6 in VARI.

pub const GLOBAL: &[&str] = &[
    "argument_relative", "argument_count", "argument", "argument0", "argument1", "argument2",
    "argument3", "argument4", "argument5", "argument6", "argument7", "argument8", "argument9",
    "argument10", "argument11", "argument12", "argument13", "argument14", "argument15",
    "debug_mode", "pointer_invalid", "pointer_null", "undefined", "room", "room_first",
    "room_last", "transition_kind", "transition_steps", "score", "lives", "health", "game_id",
    "game_display_name", "game_project_name", "game_save_id", "working_directory",
    "temp_directory", "program_directory", "instance_count", "instance_id", "room_width",
    "room_height", "room_caption", "room_speed", "room_persistent", "background_color",
    "background_showcolor", "background_colour", "background_showcolour", "background_visible",
    "background_foreground", "background_index", "background_x", "background_y",
    "background_width", "background_height", "background_htiled", "background_vtiled",
    "background_xscale", "background_yscale", "background_hspeed", "background_vspeed",
    "background_blend", "background_alpha", "view_enabled", "view_current", "view_visible",
    "view_xview", "view_yview", "view_wview", "view_hview", "view_angle", "view_hborder",
    "view_vborder", "view_hspeed", "view_vspeed", "view_object", "view_xport", "view_yport",
    "view_wport", "view_hport", "view_surface_id", "view_camera", "mouse_x", "mouse_y",
    "mouse_button", "mouse_lastbutton", "keyboard_key", "keyboard_lastkey", "keyboard_lastchar",
    "keyboard_string", "show_score", "show_lives", "show_health", "caption_score", "caption_lives",
    "caption_health", "fps", "fps_real", "current_time", "current_year", "current_month",
    "current_day", "current_weekday", "current_hour", "current_minute", "current_second",
    "event_type", "event_number", "event_object", "event_action", "error_occurred", "error_last",
    "gamemaker_registered", "gamemaker_pro", "application_surface", "os_type", "os_device",
    "os_version", "browser_width", "browser_height", "async_load", "event_data", "display_aa",
    "iap_data", "cursor_sprite", "delta_time", "webgl_enabled",
];

pub const INSTANCE: &[&str] = &[
    "x", "y", "xprevious", "yprevious", "xstart", "ystart", "hspeed", "vspeed", "direction",
    "speed", "friction", "gravity", "gravity_direction", "object_index", "id", "alarm", "solid",
    "visible", "persistent", "depth", "bbox_left", "bbox_right", "bbox_top", "bbox_bottom",
    "sprite_index", "image_index", "image_single", "image_number", "sprite_width", "sprite_height",
    "sprite_xoffset", "sprite_yoffset", "image_xscale", "image_yscale", "image_angle",
    "image_alpha", "image_blend", "image_speed", "mask_index", "path_index", "path_position",
    "path_positionprevious", "path_speed", "path_scale", "path_orientation", "path_endaction",
    "timeline_index", "timeline_position", "timeline_speed", "timeline_running", "timeline_loop",
    "phy_rotation", "phy_position_x", "phy_position_y", "phy_angular_velocity",
    "phy_linear_velocity_x", "phy_linear_velocity_y", "phy_speed_x", "phy_speed_y", "phy_speed",
    "phy_angular_damping", "phy_linear_damping", "phy_bullet", "phy_fixed_rotation", "phy_active",
    "phy_mass", "phy_inertia", "phy_com_x", "phy_com_y", "phy_dynamic", "phy_kinematic",
    "phy_sleeping", "phy_position_xprevious", "phy_position_yprevious", "phy_collision_points",
    "phy_collision_x", "phy_collision_y", "phy_col_normal_x", "phy_col_normal_y",
];
