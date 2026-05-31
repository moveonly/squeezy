import Foundation

@MainActor
public final class UserRepository {
    @Published public var users: [String] = []

    public init() {}

    public func refresh() async {
        let trimmed = "  Ada  ".sanitized()
        users.append(trimmed)
    }
}
