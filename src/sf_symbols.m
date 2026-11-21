#import <AppKit/AppKit.h>
#include <stdbool.h>
#include <stdint.h>

// Render an SF Symbol into a white/alpha mask for egui. This runs on the
// user's Mac against the system symbols: no Apple artwork is stored in the
// app, and the result never leaves the machine.

static NSImage *symbol_image(NSString *name, CGFloat point_size, NSFontWeight weight) {
    NSImage *image = [NSImage imageWithSystemSymbolName:name accessibilityDescription:nil];
    if (!image) {
        return nil;
    }
    NSImageSymbolConfiguration *configuration =
        [NSImageSymbolConfiguration configurationWithPointSize:point_size weight:weight];
    return [image imageWithSymbolConfiguration:configuration] ?: image;
}

// Largest rect that keeps the image's own proportions, centered in a square.
static NSRect aspect_fit_rect(NSImage *image, CGFloat box) {
    NSSize natural = image.size;
    if (natural.width <= 0.0 || natural.height <= 0.0) {
        return NSMakeRect(0, 0, box, box);
    }
    CGFloat scale = MIN(box / natural.width, box / natural.height);
    CGFloat width = natural.width * scale;
    CGFloat height = natural.height * scale;
    return NSMakeRect((box - width) / 2.0, (box - height) / 2.0, width, height);
}

bool rusttube_render_sf_symbol(
    const char *name,
    uint32_t size,
    bool with_check,
    uint8_t *out
) {
    if (name == NULL || size == 0 || out == NULL) {
        return false;
    }

    if (@available(macOS 11.0, *)) {
        @autoreleasepool {
        NSString *symbol_name = [NSString stringWithUTF8String:name];
        if (symbol_name == nil) {
            return false;
        }

        NSImage *image = symbol_image(symbol_name, (CGFloat)size * 0.78, NSFontWeightRegular);
        if (image == nil) {
            return false;
        }

        NSBitmapImageRep *bitmap = [[NSBitmapImageRep alloc]
            initWithBitmapDataPlanes:NULL
                          pixelsWide:size
                          pixelsHigh:size
                       bitsPerSample:8
                     samplesPerPixel:4
                            hasAlpha:YES
                            isPlanar:NO
                      colorSpaceName:NSDeviceRGBColorSpace
                         bytesPerRow:(NSInteger)size * 4
                        bitsPerPixel:32];
        if (bitmap == nil) {
            return false;
        }

        [NSGraphicsContext saveGraphicsState];
        NSGraphicsContext *context = [NSGraphicsContext graphicsContextWithBitmapImageRep:bitmap];
        [NSGraphicsContext setCurrentContext:context];
        NSRect bounds = NSMakeRect(0, 0, size, size);
        NSRectFillUsingOperation(bounds, NSCompositingOperationClear);

        // Symbols are wider or taller than they are square (the ellipsis is
        // very wide, chevrons are narrow). Fit them into the canvas instead of
        // stretching them to fill it, or their proportions bend.
        CGFloat inset = (CGFloat)size * 0.07;
        CGFloat box = (CGFloat)size - inset * 2;
        NSRect symbol_rect = aspect_fit_rect(image, box);
        symbol_rect.origin.x += inset;
        symbol_rect.origin.y += inset;
        [image drawInRect:symbol_rect
                 fromRect:NSZeroRect
                operation:NSCompositingOperationSourceOver
                 fraction:1.0
           respectFlipped:NO
                    hints:nil];

        if (with_check) {
            NSImage *check = symbol_image(@"checkmark", (CGFloat)size * 0.40, NSFontWeightSemibold);
            if (check != nil) {
                CGFloat width = (CGFloat)size * 0.40;
                NSRect check_rect = aspect_fit_rect(check, width);
                CGFloat center = (CGFloat)size * 0.50;
                check_rect.origin.x += center - NSMidX(check_rect);
                check_rect.origin.y += center - NSMidY(check_rect);
                [check drawInRect:check_rect
                         fromRect:NSZeroRect
                        operation:NSCompositingOperationDestinationOut
                         fraction:1.0
                   respectFlipped:NO
                            hints:nil];
            }
        }

        [NSGraphicsContext restoreGraphicsState];

        unsigned char *data = [bitmap bitmapData];
        if (data == NULL) {
            return false;
        }
        for (uint32_t i = 0; i < size * size; i++) {
            uint8_t alpha = data[i * 4 + 3];
            out[i * 4 + 0] = 255;
            out[i * 4 + 1] = 255;
            out[i * 4 + 2] = 255;
            out[i * 4 + 3] = alpha;
        }
        return true;
        }
    }
    return false;
}
