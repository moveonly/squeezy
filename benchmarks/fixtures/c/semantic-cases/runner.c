#include "runner.h"

int helper(int value) {
    return value + 1;
}

int runner_run(Runner *runner, int value) {
    if (value > RUNNER_LIMIT) {
        return helper(value);
    }
    return runner->id;
}
