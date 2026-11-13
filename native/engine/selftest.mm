#import <Foundation/Foundation.h>
#import <ImageIO/ImageIO.h>
#import <CoreGraphics/CoreGraphics.h>
#include <mach/mach.h>
#include <cstdio>
#include "engine.h"

int main()
{
    @autoreleasepool
    {
        engine_core_load();
        engine* e = engine_create(SELFTEST_MODEL, 320, 240);
        if (e == NULL)
        {
            fprintf(stderr, "engine_create failed\n");
            return 1;
        }
        printf("gpu=%s\n", engine_gpu_name(e));
        if (!engine_has_param(e, "ParamEyeLOpen") || engine_has_param(e, "NoSuchParam") || !engine_expression(e, 0, "F01") || engine_expression(e, 0, "no_such_expression"))
        {
            fprintf(stderr, "param or expression lookup wrong\n");
            return 1;
        }
        engine_expressions_stop(e);
        const uint8_t* rgba = NULL;
        for (int i = 0; i < 24; i++)
        {
            engine_update(e, 1000.0f / 12.0f, NULL, NULL, NULL);
            rgba = engine_render(e, i * 1000.0f / 12.0f);
        }
        long lit = 0;
        for (int p = 0; p < 320 * 240; p++)
        {
            lit += rgba[p * 4] + rgba[p * 4 + 1] + rgba[p * 4 + 2] > 60;
        }
        printf("lit_pixels=%ld\n", lit);
        CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        CGContextRef context = CGBitmapContextCreate((void*)rgba, 320, 240, 8, 320 * 4, space, kCGImageAlphaNoneSkipLast | kCGBitmapByteOrder32Big);
        CGImageRef image = CGBitmapContextCreateImage(context);
        NSURL* url = [NSURL fileURLWithPath:[[[NSString stringWithUTF8String:__FILE__] stringByDeletingLastPathComponent] stringByAppendingPathComponent:@"build/selftest.png"]];
        CGImageDestinationRef destination = CGImageDestinationCreateWithURL((__bridge CFURLRef)url, CFSTR("public.png"), 1, NULL);
        CGImageDestinationAddImage(destination, image, NULL);
        bool written = CGImageDestinationFinalize(destination);
        CFRelease(destination);
        CGImageRelease(image);
        CGContextRelease(context);
        CGColorSpaceRelease(space);
        static const uint8_t white_frame[] = {0x21, 0xf9, 0x04, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00};
        static const uint8_t gif_header[] = {'G', 'I', 'F', '8', '9', 'a', 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
        NSMutableData* gif = [NSMutableData dataWithBytes:gif_header length:sizeof(gif_header)];
        [gif appendBytes:white_frame length:sizeof(white_frame)];
        [gif appendBytes:white_frame length:sizeof(white_frame)];
        [gif appendBytes:";" length:1];
        int background_frames = engine_background_load(e, (const uint8_t*)[gif bytes], [gif length], "image/gif");
        const uint8_t* with_background = engine_render(e, 0);
        long lit_background = 0;
        for (int p = 0; p < 320 * 240; p++)
        {
            lit_background += with_background[p * 4] + with_background[p * 4 + 1] + with_background[p * 4 + 2] > 60;
        }
        engine_background_clear(e);
        printf("background_frames=%d lit_with_background=%ld\n", background_frames, lit_background);
        mach_task_basic_info_data_t task_info_data = {};
        mach_msg_type_number_t task_info_count = MACH_TASK_BASIC_INFO_COUNT;
        task_info(mach_task_self(), MACH_TASK_BASIC_INFO, (task_info_t)&task_info_data, &task_info_count);
        printf("rss_kb=%llu\n", task_info_data.resident_size / 1024);
        if (!written || lit < 1000 || background_frames < 2 || lit_background <= lit || engine_background_load(e, (const uint8_t*)"junk", 4, "image/png") != 0)
        {
            fprintf(stderr, "selftest failed written=%d lit=%ld background_frames=%d\n", written, lit, background_frames);
            return 1;
        }
        engine_destroy(e);
        printf("selftest ok %s\n", [[url path] UTF8String]);
    }
    return 0;
}
