// Native window chrome helpers.
//
// The app draws its own top bar into the titlebar area, so the traffic lights
// are the only native controls left. They start hidden and are re-shown when
// the pointer comes near their corner, the way macOS' own auto-hide titlebar
// behaves. AppKit only lets the buttons be hidden after the window exists,
// which is why this cannot be done through the viewport builder alone.

#import <Cocoa/Cocoa.h>

void rusttube_set_traffic_lights_visible(int visible) {
    @autoreleasepool {
        NSApplication *app = [NSApplication sharedApplication];
        NSWindow *window = [app keyWindow] ?: [app mainWindow];
        if (window == nil) {
            for (NSWindow *candidate in [app windows]) {
                if ([candidate isVisible]) {
                    window = candidate;
                    break;
                }
            }
        }
        if (window == nil) {
            return;
        }

        NSWindowButton buttons[3] = {
            NSWindowCloseButton,
            NSWindowMiniaturizeButton,
            NSWindowZoomButton,
        };
        for (int i = 0; i < 3; i++) {
            NSButton *button = [window standardWindowButton:buttons[i]];
            button.hidden = (visible == 0);
        }
    }
}
