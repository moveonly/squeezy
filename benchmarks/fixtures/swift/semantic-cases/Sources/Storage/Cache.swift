import Foundation

public actor Cache<Key: Hashable, Value> {
    private var storage: [Key: Value] = [:]

    public init() {}

    public func get(_ key: Key) async -> Value? {
        return storage[key]
    }

    public func set(_ key: Key, _ value: Value) async {
        storage[key] = value
    }
}
