//! Short hover descriptions: what a command does, in plain words. Menus,
//! toolbars and buttons show them as tooltips (with the shortcut, if any).
//! Commands whose label already says everything have no description.

use super::commands::Command::{self, *};

/// One plain sentence about `c`, or "" when the label says it all.
pub fn describe(c: Command) -> &'static str {
    match c {
        OpenFile => "Open an image or video from disk",
        OpenFolder => "Show the images of a folder in the browser",
        CommandPalette => "Search every command by name and run it",
        Settings => "Appearance, viewer, browser, keyboard and file options",
        ShortcutsHelp => "List every keyboard shortcut",
        ToggleFullscreen => "Fill the whole screen with Lumen (Esc to leave)",
        ToggleTheme => "Switch between the light and dark look",
        Refresh => "Reload the folder from disk",
        GoUp => "Open the folder that contains this one",
        GoBack => "Go to the folder you were in before",
        GoForward => "Return to the folder you went back from",
        InvertSelection => "Select the unselected files and unselect the selected ones",
        CopyTo => "Copy the selected files into another folder",
        MoveTo => "Move the selected files into another folder",
        Delete => "Move the selected files to the Recycle Bin (Undo puts them back)",
        Rename => "Give the selected file a new name",
        AddFavorite => "Pin this folder to Favorites in the folder tree",
        ShowInExplorer => "Open Windows Explorer with this file selected",
        Share => "Send the files by mail, to a nearby PC or to another app (Windows Share)",
        CopyPath => "Copy the full file path as text",
        CutFiles => "Cut the selected files: paste them in another folder or in Explorer to move them",
        CopyFiles => "Copy the selected files so you can paste them in Explorer",
        PasteFiles => "Paste files copied or cut in Explorer or Lumen into this folder (cut files are moved)",
        ToggleDetails => "Switch between the thumbnail grid and a list with file details",
        TogglePreviewPane => "Show a larger preview of the selected image under the folder tree",
        FocusFilter => "Type to show only files whose names match",
        BatchConvert => "Convert, resize or rename many images at once",
        BatchRename => "Rename many files at once with a pattern",
        ContactSheet => "Lay out thumbnails of many images on one page",
        FindDuplicates => "Find identical and look-alike images in this folder",
        Slideshow => "Show the images full screen one after another",
        Compare => "Show 2 to 4 selected images side by side with the same zoom",
        ToggleFlag => "Mark the image as a pick; show only flagged files with Flagged only",
        ShowFlaggedOnly => "Hide every file that is not flagged",
        OpenVideoMode => "Step through videos frame by frame and compare them side by side",
        OpenViewer => "Open the selected image in the viewer",
        CloseViewer => "Return to the folder's thumbnails",
        ZoomActual => "Show one image pixel per screen pixel",
        ZoomFit => "Fit the whole image in the window",
        CycleFit => "Step through fit to window, fill, fit width and fit height",
        FitWidth => "Make the image as wide as the window",
        FitHeight => "Make the image as tall as the window",
        Crop => "Cut away the edges of the image",
        Resize => "Change the image's size in pixels",
        CanvasSize => "Add space around the image or trim it",
        AdjustColors => "Brightness, contrast, saturation, warmth and more",
        Levels => "Set the black point, white point and midtones",
        Curves => "Fine-tune brightness and contrast with a curve",
        Sharpen => "Make edges and fine detail crisper",
        Blur => "Soften the whole image",
        Effects => "Black and white, sepia, vignette and other looks",
        AutoEnhance => "Fix brightness, contrast and color in one step",
        AddBorder => "Put a border or frame around the image",
        Watermark => "Stamp text or a logo on the image",
        FreeRotate => "Rotate by any angle to straighten a tilted horizon",
        Save => "Save the changes into this file",
        SaveAs => "Save a copy under another name or format",
        CopyImage => "Copy the picture so you can paste it in other apps",
        PasteImage => "Save the picture on the clipboard as a new PNG next to this one and open it",
        SetWallpaper => "Use this image as the desktop background",
        ToggleInfo => "Show file details, camera settings and the histogram",
        ToggleHistogram => "Show how dark and bright tones are spread in the image",
        ToggleMagnifier => "A loupe that enlarges the area under the mouse",
        ToggleFilmstrip => "A strip of the folder's thumbnails under the image",
        PlayPause => "Play or pause the video or animated image on screen",
        NextFrame => "Show the next frame of a paused video or animation",
        PrevFrame => "Show the previous frame of a paused video or animation",
        SeekBack => "Go 5 seconds back in the video on screen",
        SeekForward => "Go 5 seconds on in the video on screen",
        ToggleMute => "Turn the sound of videos in the viewer off or on",
        OpenWithEditor => "Open the file in the editor set in Settings > External editors",
        OpenWithEditorN(_) => "Open the file in another editor set in Settings > External editors",
        Properties => "Show file, camera and metadata details",
        OpenGpsMap => "Open the place the photo was taken in OpenStreetMap (online)",
        ToggleLockZoom => "Keep the same zoom when moving to the next image",
        EditComment => "Write a description stored in the file's metadata",
        RemoveMetadata => "Strip camera, location and other hidden data from files",
        ChangeDates => "Change the file and capture dates",
        ExportXmp => "Write ratings, flags and labels into .xmp files next to the images",
        CreateMultiPage => "Combine images into one PDF, TIFF or animated GIF",
        ImageStrip => "Join images into one picture, side by side or stacked",
        CreateZip => "Pack the selected files into a ZIP archive",
        DrawBoard => "Draw arrows, boxes, circles, text and highlights on the image",
        CloneStamp => "Paint over a spot with pixels copied from elsewhere",
        HealingBrush => "Remove spots and blemishes so they blend in",
        RedEye => "Fix red eyes from a camera flash",
        BlurRegion => "Blur or pixelate a part of the image (faces, plates)",
        Selection => "Pick an area to edit, copy or crop",
        ToggleAnalysis => "Quality measurements (sharpness, noise, exposure) and AI review",
        RunMetrics => "Measure sharpness, noise, exposure and other quality numbers",
        RunAi => "Ask your AI server to describe the image's problems",
        OpenProject => "Pick a folder of captures from several phones: scenes are paired for review, scoring and one report",
        Workbench => "Treat this folder as a camera comparison project: devices and scenes are found automatically",
        BlindTest => "Judge random pairs of the project with names hidden; devices are ranked from the votes",
        MeasureProject => "Measure sharpness, noise, clipping and color of every capture in the project",
        ShowExif => "Every EXIF tag of the image; in Compare all images side by side with differences marked",
        ShowMetadata => "Capture features (HDR, night, zoom), EXIF, encoding, color profile; opens on the tab used last",
        TogglePixelValues => "Show RGB, hex, L*a*b* and luma of the pixel under the pointer (in Compare: V)",
        ExportMetadataCsv => "One row per image: camera features, exposure, every EXIF tag, JPEG quality",
        CopyImageText => "Read the text in the picture (screenshots, signs, documents) and copy it",
        FocusAddress => "Edit the address bar: type or paste a folder or file path and press Enter",
        SelectFlagged => "Select the flagged files in the folder, ready to copy, move or compare",
        MoveEarlier => "Arrange the folder by hand: the selected files move one place earlier",
        MoveLater => "Arrange the folder by hand: the selected files move one place later",
        CycleInspect => "Cycle clipping, false color, focus peaking, channels and HDR gain map views (in Compare: O)",
        _ => "",
    }
}

/// Tooltip text for `c`: its description (or label) and its shortcut.
pub fn command_tip(c: Command) -> String {
    let d = describe(c);
    let base = if d.is_empty() { c.label().trim_end_matches("...").to_string() } else { d.to_string() };
    with_shortcut(&base, &c.shortcut())
}

/// "text  (shortcut)", or just the text when there is no shortcut.
pub fn with_shortcut(text: &str, shortcut: &str) -> String {
    if shortcut.is_empty() {
        text.to_string()
    } else {
        format!("{text}  ({shortcut})")
    }
}

/// Add `c`'s description to a menu row (rows whose label says it all get none).
pub fn menu_tip(resp: egui::Response, c: Command) -> egui::Response {
    let d = describe(c);
    if d.is_empty() {
        resp
    } else {
        resp.on_hover_text(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptions_are_short_and_plain() {
        for c in Command::all() {
            let d = describe(c);
            assert!(d.len() <= 100, "{c:?}: too long");
            assert!(!d.ends_with('.'), "{c:?}: no full stop");
        }
        assert_eq!(with_shortcut("Zoom in", ""), "Zoom in");
        assert_eq!(with_shortcut("Zoom in", "+"), "Zoom in  (+)");
    }
}
