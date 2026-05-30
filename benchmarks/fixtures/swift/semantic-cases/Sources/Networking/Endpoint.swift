import Foundation

/// Request-endpoint protocol exercised by `UserEndpoint`.
public protocol Endpoint {
    var path: String { get }
    func encode() -> Data
}

public struct UserEndpoint: Endpoint {
    public let path: String = "/users"

    public init() {}

    public func encode() -> Data {
        return Data(path.utf8)
    }
}
