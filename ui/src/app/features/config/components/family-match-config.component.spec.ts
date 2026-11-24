import { TestBed } from '@angular/core/testing';
import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting, HttpTestingController } from '@angular/common/http/testing';
import { FamilyMatchConfigComponent } from './family-match-config.component';

describe('FamilyMatchConfigComponent', () => {
  let httpMock: HttpTestingController;

  beforeEach(() => {
    TestBed.configureTestingModule({ providers: [provideHttpClient(), provideHttpClientTesting()] });
    httpMock = TestBed.inject(HttpTestingController);
  });

  afterEach(() => httpMock.verify());

  function create() {
    const fixture = TestBed.createComponent(FamilyMatchConfigComponent);
    fixture.detectChanges();
    return fixture;
  }

  it('should show the mode the server reports', () => {
    const fixture = create();
    httpMock.expectOne('/api/family-match').flush({ family_match: 'newest' });
    fixture.detectChanges();

    expect(fixture.componentInstance.mode()).toBe('newest');
  });

  it('should keep the user choice when the initial GET resolves after a change', () => {
    const fixture = create();
    const get = httpMock.expectOne('/api/family-match');

    fixture.componentInstance.onChange('newest');
    get.flush({ family_match: 'shortest' });
    httpMock.expectOne('/api/family-match').flush({ family_match: 'newest' });

    expect(fixture.componentInstance.mode()).toBe('newest');
  });

  it('should POST the new mode when the select changes', () => {
    const fixture = create();
    httpMock.expectOne('/api/family-match').flush({ family_match: 'shortest' });

    fixture.componentInstance.onChange('newest');
    const post = httpMock.expectOne('/api/family-match');
    expect(post.request.method).toBe('POST');
    expect(post.request.body).toEqual({ family_match: 'newest' });
    post.flush({ family_match: 'newest' });

    expect(fixture.componentInstance.mode()).toBe('newest');
    expect(fixture.componentInstance.saving()).toBeFalse();
  });

  it('should revert and show the server error when saving fails', () => {
    const fixture = create();
    httpMock.expectOne('/api/family-match').flush({ family_match: 'shortest' });

    fixture.componentInstance.onChange('newest');
    httpMock
      .expectOne('/api/family-match')
      .flush({ error: 'failed to persist runtime overrides: disk full' }, { status: 500, statusText: 'Server Error' });
    fixture.detectChanges();

    expect(fixture.componentInstance.mode()).toBe('shortest');
    expect(fixture.componentInstance.error()).toContain('disk full');
  });
});
