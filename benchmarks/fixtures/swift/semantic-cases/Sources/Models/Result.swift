import Foundation

public enum APIResult<Value, Failure: Error> {
    case success(Value)
    case failure(Failure)

    public func map<New>(_ transform: (Value) -> New) -> APIResult<New, Failure> {
        switch self {
        case .success(let value):
            return .success(transform(value))
        case .failure(let error):
            return .failure(error)
        }
    }
}

public typealias Endpoints = [String]
