// Nonposting native validation; no application, window, display or input.
#import <ApplicationServices/ApplicationServices.h>
#include "browser-pointer.h"
#include <stdio.h>
#include <string.h>
int main(int argc,const char **argv) {
    @autoreleasepool {
        (void)browser_pointer_send; // Compile the real sender, but never call it.
        if (argc==2 && strcmp(argv[1],"--pipe")==0) {
            BrowserPointerPlan p={0}; BOOL valid=browser_pointer_read(&p) && browser_pointer_valid(p);
            printf("{\"valid\":%s,\"posted_events\":0}\n",valid?"true":"false");
            return valid?0:1;
        }
        if (argc!=1) return 2;
        BrowserPointerPlan good={1,123,{{-770,30},{720,530}},{-570,257},{200,227},777};
        BOOL ok=browser_pointer_valid(good); unsigned cases=1;
        for(unsigned i=0;i<16;i++) {
            BrowserPointerPlan p=good;
            switch(i) {
                case 0:p.version=0;break;
                case 1:p.window=0;break;
                case 2:p.tag=0;break;
                case 3:p.tag=-1;break;
                case 4:p.bounds.size.width=0;break;
                case 5:p.bounds.size.height=16385;break;
                case 6:p.global.x=NAN;break;
                case 7:p.local.y=INFINITY;break;
                case 8:p.local.y=303;break; // Wrong bottom-left origin, not center.
                case 9:p.local.x+=1;break;
                case 10:p.global.y=31;p.local.y=1;break;
                case 11:p.global.x=-771;p.local.x=-1;break;
                case 12:p.bounds.origin.x=1000001;break;
                case 13:p.bounds.size.height=-530;break;
                case 14:p.global.x=INFINITY;break;
                case 15:p.local.x=-INFINITY;break;
            }
            ok &= !browser_pointer_valid(p);cases++;
        }
        ok &= NSApp==nil;
        printf("{\"ok\":%s,\"cases\":%u,\"posted_events\":0,\"application_created\":false}\n",ok?"true":"false",cases);
        return ok?0:1;
    }
}
