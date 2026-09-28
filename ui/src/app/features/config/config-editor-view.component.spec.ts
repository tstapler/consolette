import { TestBed } from '@angular/core/testing';
import { ConfigEditorViewComponent } from './config-editor-view.component';
import { ConfigStore } from '../../core/stores/config.store';
import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting, HttpTestingController } from '@angular/common/http/testing';

describe('ConfigEditorViewComponent (TC-UNIT-32)', () => {
  let store: ConfigStore;
  let httpMock: HttpTestingController;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [ConfigEditorViewComponent],
      providers: [
        ConfigStore,
        provideHttpClient(),
        provideHttpClientTesting()
      ]
    }).compileComponents();

    store = TestBed.inject(ConfigStore);
    httpMock = TestBed.inject(HttpTestingController);
  });

  afterEach(() => {
    httpMock.verify();
  });

  it('TC-UNIT-32: should trigger Apply Config action and handle notifications', () => {
    const fixture = TestBed.createComponent(ConfigEditorViewComponent);
    fixture.detectChanges();

    const getReq = httpMock.expectOne('/v1/dashboard/config');
    getReq.flush({
      webUi: 'angular',
      fallbackCascade: ['anthropic', 'openai']
    });
    fixture.detectChanges();

    const component = fixture.componentInstance;
    component.onApplyConfig();

    const putReq = httpMock.expectOne('/v1/dashboard/config');
    expect(putReq.request.method).toBe('PUT');
    putReq.flush({
      webUi: 'angular',
      fallbackCascade: ['anthropic', 'openai']
    });
    fixture.detectChanges();

    expect(component.showToast()).toBeTrue();
  });
});
