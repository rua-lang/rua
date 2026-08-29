/* Embedding rua in a C program. Build and run: sh demo/run.sh */
#include <stdio.h>
#include "rua.h"

/* a C function that rua scripts can call */
static double c_hypot(const double *a, int n) {
    if (n < 2) return 0;
    return __builtin_sqrt(a[0] * a[0] + a[1] * a[1]);
}

int main(void) {
    rua_State *S = rua_new();

    rua_register(S, "hypot", c_hypot);
    rua_set_number(S, "scale", 3.0);

    if (rua_eval(S,
        "fn area(r) {\n"
        "  math::pi * r * r * scale\n"
        "}\n"
        "return hypot(3, 4), \"from rua\";\n") != 0) {
        fprintf(stderr, "rua: %s\n", rua_error(S));
        return 1;
    }

    printf("hypot(3,4)      = %g\n", rua_result_number(S, 0));
    printf("second result   = %s\n", rua_result_string(S, 1));

    double out = 0;
    double args[1] = { 2.0 };
    if (rua_call(S, "area", args, 1, &out) == 0)
        printf("area(2)*scale   = %.4f\n", out);

    /* errors come back as messages, not crashes */
    if (rua_eval(S, "nosuchfunction()") != 0)
        printf("expected error  = %s\n", rua_error(S));

    rua_close(S);
    return 0;
}
