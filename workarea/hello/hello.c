/* Hello world for RISC iX 1.21 (4.3BSD, 1990 Norcroft cc): pre-ANSI style.
 *
 * Build and run from this directory on the host:
 *   dockerix exec cc -o hello hello.c && dockerix exec ./hello
 * or, without the wrapper, from the work-area root:
 *   docker exec dockerix-vm rix sh -c 'cd hello && cc -o hello hello.c && ./hello'
 */
#include <stdio.h>

int main()
{
    printf("Hello, world from RISC iX\n");
    return 0;
}
