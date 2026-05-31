import 'package:fixture/src/services/service.dart';
import 'package:fixture/src/util/string_ext.dart';

void main() async {
  final s = Service();
  await s.run();
  print('hi'.shout());
}
