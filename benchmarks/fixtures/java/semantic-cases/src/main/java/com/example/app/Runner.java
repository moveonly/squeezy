package com.example.app;

import com.example.services.FriendlyGreeter;
import com.example.services.Greeter;
import static com.example.util.Names.defaultName;

public class Runner extends BaseRunner implements Runnable {
    private final Greeter greeter;

    public Runner(Greeter greeter) {
        this.greeter = greeter;
    }

    @Override
    public void run() {
        String name = prepareName(defaultName());
        greeter.greet(name);
        new Helper().assist();
    }

    public static Runner buildDefault() {
        return new Runner(new FriendlyGreeter());
    }
}

class BaseRunner {
    protected String prepareName(String raw) {
        return raw.trim();
    }
}

record Helper() {
    void assist() {}
}
