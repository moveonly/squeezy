import 'package:fixture/src/util/loggable.dart' show Loggable;
import 'package:fixture/src/network/client.dart' as net;

class Service with Loggable {
  Future<void> run() async {
    final c = net.HttpClient();
    final r = await c.fetch('/x');
    log('got ${r.status}');
  }
}
