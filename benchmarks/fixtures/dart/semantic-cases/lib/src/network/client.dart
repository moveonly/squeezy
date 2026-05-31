library network.client;

part 'response.dart';

class HttpClient {
  HttpClient();

  Future<Response> fetch(String url) async {
    final body = '{"status":200,"body":"$url"}';
    return Response(200, body);
  }
}
